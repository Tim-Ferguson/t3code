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
/// A transfer or handoff row of a planned run, validated before it is committed.
pub(crate) fn checked_transfer_row(kind: &str, row: Value) -> Result<Value, StoreError> {
    if kind == "context-handoff.updated" {
        checked::<t3_contracts::ContextHandoff>(row)
    } else {
        checked::<t3_contracts::ContextTransfer>(row)
    }
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
        crate::provider_switch::register(&store, &providers);
        crate::restart_continuation::started(&store);
        // The turns this start finds cut get their continuation before recovery cancels them.
        crate::restart_continuation::queue_after_crash(&store, &providers, &store.projections("thread")?);
        crate::codex_runtime::recover(&store, &lease)?;
        let runtime = crate::codex_runtime::start(store.clone(), providers.clone(), lease);
        let execution = Self {
            store,
            providers,
            runtime,
        };
        // Finalizes delegated tasks whose runs end, and delivers their completion to the parent.
        if tokio::runtime::Handle::try_current().is_ok() {
            crate::usage_limits::start(&execution);
            crate::thread_settlement::start(&execution);
            crate::delegated_completion::start_listener(&execution);
        }
        Ok(execution)
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
    /// Progress and cancellation of worktree setups, and the terminals setup scripts run in.
    pub fn workspace(&self) -> std::sync::Arc<crate::workspace_prepare::WorkspaceRuntime> {
        self.runtime.workspace().clone()
    }
    /// Returns once the thread has no effect left to run or running: its queued turn starts,
    /// cleanups and detaches have all settled. For callers that must observe what an effect did.
    pub async fn drained(&self, thread_id: &str) {
        let mut settled = self.runtime.subscribe_settled();
        loop {
            let busy: u64 = self
                .store
                .read(|connection| {
                    Ok(connection.query_row(
                        "SELECT COUNT(*) FROM rust_effect_outbox WHERE thread_id=?1 AND status IN ('pending','running')",
                        [thread_id],
                        |row| row.get(0),
                    )?)
                })
                .unwrap_or(0);
            if busy == 0 || settled.changed().await.is_err() {
                return;
            }
        }
    }
    /// Wakes the effect loop after a command committed outside `dispatch_guarded`.
    pub(crate) fn wake(&self) {
        self.runtime.wake();
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
        // While the providers are live: which turns the shutdown is about to cut.
        crate::restart_continuation::prepare_for_shutdown(&self.store, &self.providers);
        crate::provider_switch::unregister(&self.store);
        self.runtime.shutdown().await;
    }
    /// Await cleanup of the currently admitted actors for these instances.
    /// This sends no stop signal; callers first cause their sessions to close.
    /// Removed actors have already completed their process and credential cleanup.
    pub async fn wait_provider_cleanup(&self, instances: &[String]) {
        self.runtime.wait_instances(instances).await;
    }
    pub fn dispatch(&self, input: &Value, now: DateTime<Utc>) -> Result<Receipt, StoreError> {
        self.dispatch_guarded(input, now, |_, _| Ok(()))
    }
    /// Final trusted admission runs against the exact command planning state.
    pub fn dispatch_guarded(
        &self,
        input: &Value,
        now: DateTime<Utc>,
        authorize: impl Fn(&rusqlite::Transaction<'_>, &Value) -> Result<(), StoreError>,
    ) -> Result<Receipt, StoreError> {
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
                authorize(transaction, &projection)?;
                match kind {
                    "message.dispatch" => {
                        // The provider owns a native subagent's conversation: a message has nowhere to go.
                        if crate::subagents::is_provider_native_subagent_thread(&projection["thread"]) {
                            return Err(StoreError::InvalidCommand(crate::subagents::READ_ONLY_REASON.into()));
                        }
                        let model = command
                            .get("modelSelection")
                            .unwrap_or(&projection["thread"]["modelSelection"]);
                        let driver = self
                            .providers
                            .driver(model["instanceId"].as_str().unwrap())
                            .map_err(|error| StoreError::InvalidCommand(error.to_string()))?;
                        // Another instance of the same provider continues the native conversation as it is
                        // (`canResumeAcrossInstances`); only this planner may say so, never the client.
                        let mut command = command.clone();
                        let active = projection["providerThreads"].as_array().into_iter().flatten().find(|row| row["id"] == projection["thread"]["activeProviderThreadId"]);
                        if active.is_some_and(|row| row["providerInstanceId"] != model["instanceId"] && !row["nativeThreadRef"].is_null() && self.providers.continues_across(row["providerInstanceId"].as_str().unwrap_or(""), model["instanceId"].as_str().unwrap_or(""))) {
                            command["resumeAcrossInstances"] = json!(true);
                        }
                        plan_message_in_transaction(&command, &projection, transaction, now, driver)
                    }
                    "checkpoint.rollback" => {
                        let instance = projection["thread"]["modelSelection"]["instanceId"]
                            .as_str()
                            .unwrap_or("");
                        let driver = self
                            .providers
                            .driver(instance)
                            .map_err(|error| StoreError::InvalidCommand(error.to_string()))?;
                        let capabilities = crate::checkpoint_rollback::driver_capabilities(driver)
                            .ok_or_else(|| {
                                StoreError::InvalidCommand(format!(
                                    "{instance} cannot satisfy rollback for command {}: conversation rollback is unavailable",
                                    command["commandId"].as_str().unwrap_or("")
                                ))
                            })?;
                        crate::checkpoint_rollback::plan(&command, &projection, transaction, now, &capabilities)
                    }
                    "run.interrupt" => plan_interrupt(&command, &projection, now),
                    "thread.stop" => plan_thread_stop(&command, &projection, now),
                    "queued-message.promote-to-steer" => crate::message_steering::plan_promote(&command, &projection, now, Some(transaction)),
                    "queued-run.cancel" => crate::queued_runs::plan_cancel(&command, &projection, now),
                    "queued-run.edit" => crate::queued_runs::plan_edit(&command, &projection, now),
                    "queued-run.reorder" => crate::queued_runs::plan_reorder(&command, &projection, now),
                    "queue.resume" => crate::queued_runs::plan_resume(&command, &projection, now),
                    "runtime-request.respond" => plan_response(&command, &projection, now),
                    "prepared-run.release" => plan_release_in(&command, &projection, now, Some(transaction)),
                    "prepared-run.progress" => crate::prepared_run::plan_progress(&command, &projection, now),
                    "prepared-run.fail" => crate::prepared_run::plan_fail(&command, &projection, now),
                    "prepared-run.retry" => crate::prepared_run::plan_retry(&command, &projection, now),
                    _ => Err(StoreError::InvalidCommand(format!(
                        "Native provider command {kind} is not yet available."
                    ))),
                }
            },
            thread::reduce,
        )?;
        if kind == "queue.resume" {
            // The released queue starts its next run; a replayed command finds it already started.
            crate::message_queue::settle(&self.store, thread_id, None)?;
        }
        if kind == "run.interrupt" && !replayed {
            self.runtime
                .cancel_start(thread_id, command["runId"].as_str().unwrap());
        }
        if kind == "thread.stop" && !replayed {
            // A run still preparing when Stop reached it never starts.
            if let Some(run) = self.store.projection("thread", thread_id)?.and_then(|view| view["runs"].as_array().and_then(|runs| runs.iter().rfind(|run| run["status"] == "interrupted" || run["status"] == "cancelled").cloned())) {
                self.runtime.cancel_start(thread_id, run["id"].as_str().unwrap_or_default());
            }
        }
        self.runtime.wake();
        Ok(receipt)
    }
}

pub(crate) fn effect(command: &Value, request: Value, now: DateTime<Utc>) -> NewEffect {
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

fn encode_id(value: &str) -> String {
    let mut result = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            result.push(byte as char);
        } else {
            use std::fmt::Write;
            write!(result, "%{byte:02X}").unwrap();
        }
    }
    result
}
fn user_item_id(message_id: &str) -> String {
    format!("turn-item:message:{}", encode_id(message_id))
}
pub(crate) fn run_signal_item_id(run_id: &str, signal: &str) -> String {
    format!(
        "turn-item:run:{}:signal:{}",
        encode_id(run_id),
        encode_id(signal)
    )
}

fn supports_queued_messages(session: Option<&Value>, driver: &str) -> bool {
    if let Some(capabilities) = session
        .and_then(|session| session.get("capabilities"))
        .filter(|capabilities| !capabilities.is_null())
    {
        return capabilities["turns"]["supportsQueuedMessages"] == true;
    }
    // A prepared run has no negotiated session yet. Admission uses the actual
    // selected adapter's default capabilities, exactly as the original does.
    let encoded = match driver {
        "codex" => include_str!("codex-capabilities.json"),
        "claudeAgent" => include_str!("claude-capabilities.json"),
        "acpRegistry" | "antigravity" => include_str!("acp-capabilities.json"),
        "grok" => include_str!("grok-capabilities.json"),
        _ => return false,
    };
    let capabilities: Value = serde_json::from_str(encoded).expect("static provider capabilities");
    capabilities["turns"]["supportsQueuedMessages"] == true
}
/// `thread.stop`: the Stop the UI gives a thread. Its active run is interrupted with the queue held;
/// a thread with none still has its queued turns held and its pull request watches ended.
fn plan_thread_stop(command: &Value, projection: &Value, now: DateTime<Utc>) -> Result<Decision, StoreError> {
    let active = projection["runs"].as_array().into_iter().flatten().filter(|run| crate::message_queue::blocking(run)).max_by_key(|run| run["ordinal"].as_u64().unwrap_or(0));
    let Some(run) = active else {
        return Ok(Decision::Accepted { events: crate::delegated_tasks::stop_idle_thread(command, projection, now)?, effects: vec![] });
    };
    let mut interrupt = json!({"type":"run.interrupt","commandId":command["commandId"],"threadId":command["threadId"],"runId":run["id"],"holdQueue":true});
    if let Some(reason) = command.get("reason").filter(|reason| !reason.is_null()) {
        interrupt["reason"] = reason.clone();
    }
    plan_interrupt(&interrupt, projection, now)
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
    // A run waiting for its checkpoint has finished its turn: only its background work is left.
    if !matches!(
        run["status"].as_str(),
        Some("preparing" | "starting" | "running" | "waiting")
    ) || (run["status"] == "waiting" && crate::background_work::run_owns_pending_work(projection, run))
    {
        // A settled run can still own background work; Stop ends that.
        return crate::background_work::plan_stop(command, projection, run, now);
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
    let mut item = json!({"id":run_signal_item_id(run["id"].as_str().unwrap(),"interrupt-request"),"threadId":thread_id,"runId":run["id"],"nodeId":run["rootNodeId"],"providerThreadId":run["providerThreadId"],"providerTurnId":attempt["providerTurnId"],"nativeItemRef":null,"parentItemId":null,"ordinal":projection["turnItems"].as_array().unwrap().iter().filter_map(|item|item["ordinal"].as_u64()).max().map_or(0,|ordinal|ordinal+1),"status":"completed","title":"Interrupt requested","startedAt":timestamp,"completedAt":timestamp,"updatedAt":timestamp,"type":"run_interrupt_request","message":command.get("reason").cloned().unwrap_or(json!("Interrupt requested"))});
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
    events.extend(crate::delegated_tasks::stop_remaining(command, projection, run, now)?);
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
        item["id"] = json!(run_signal_item_id(
            run["id"].as_str().unwrap(),
            "interrupt-result"
        ));
        item["parentItemId"] = json!(run_signal_item_id(
            run["id"].as_str().unwrap(),
            "interrupt-request"
        ));
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
        // An answered question keeps what was answered, with the files attached to each answer.
        if item["type"] == "user_input_request" && command.get("answers").is_some() {
            let question_text: serde_json::Map<String, Value> = item["questions"].as_array().into_iter().flatten().map(|question| (question["id"].as_str().unwrap_or("").to_owned(), question["question"].clone())).collect();
            item["questionAnswer"] = json!({"requestId":command["requestId"],"answers":command["answers"],"attachmentsByQuestionId":command.get("attachmentsByQuestionId").cloned().unwrap_or(json!({})),"questionTextById":question_text});
        }
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
    plan_message_inner(command, projection, now, driver, false, None)
}

pub(crate) fn plan_message_in_transaction(
    command: &Value,
    projection: &Value,
    transaction: &rusqlite::Transaction<'_>,
    now: DateTime<Utc>,
    driver: &str,
) -> Result<Decision, StoreError> {
    plan_message_inner(command, projection, now, driver, false, Some(transaction))
}

fn plan_message_inner(
    command: &Value,
    projection: &Value,
    now: DateTime<Utc>,
    driver: &str,
    reuse_message: bool,
    transaction: Option<&rusqlite::Transaction<'_>>,
) -> Result<Decision, StoreError> {
    // A message to a settled or snoozed thread brings it back first. The automatic continuations decide
    // for themselves whether they still apply, and a stale one changes nothing.
    let automatic = command.get("usageLimitContinuationOfRunId").is_some() || command.get("restartContinuationOfRunId").is_some();
    let unparked = if automatic { None } else { crate::thread_settlement::unpark(command["commandId"].as_str().unwrap_or_default(), projection, now) };
    let (projection, brought_back) = match &unparked {
        Some((adjusted, events)) => (adjusted, events.clone()),
        None => (projection, vec![]),
    };
    let decision = plan_message_titled(command, projection, now, driver, reuse_message, transaction)?;
    Ok(match decision {
        Decision::Accepted { mut events, effects } if !brought_back.is_empty() => {
            events.splice(0..0, brought_back);
            Decision::Accepted { events, effects }
        }
        other => other,
    })
}

fn plan_message_titled(
    command: &Value,
    projection: &Value,
    now: DateTime<Utc>,
    driver: &str,
    reuse_message: bool,
    transaction: Option<&rusqlite::Transaction<'_>>,
) -> Result<Decision, StoreError> {
    // The first message of a thread names it: the seed now, a model's title once this commits.
    let Some(title) = crate::text_generation_title::first_message_title(command, projection, reuse_message, now) else {
        return plan_message_body(command, projection, now, driver, reuse_message, transaction);
    };
    let decision = plan_message_body(command, &title.projection, now, driver, reuse_message, transaction)?;
    Ok(title.attach(decision))
}

/// The checks that apply to a message whatever way it is delivered, and the completion of the
/// proposed plan it implements; the delivery itself is `plan_message_delivery`.
fn plan_message_body(
    command: &Value,
    projection: &Value,
    now: DateTime<Utc>,
    driver: &str,
    reuse_message: bool,
    transaction: Option<&rusqlite::Transaction<'_>>,
) -> Result<Decision, StoreError> {
    check_manual_continuation(command, projection)?;
    let source_plan = source_plan_completion(command, projection, transaction)?;
    let mut decision = plan_message_delivery(command, projection, now, driver, reuse_message, transaction)?;
    if let Some((plan_thread, plan)) = source_plan {
        // The plan completes right after the run it starts is created. A steered message
        // creates no run and leaves the plan as it was.
        let command_id = command["commandId"].as_str().unwrap_or("");
        if let Decision::Accepted { events, .. } | Decision::AcceptedBatch { events, .. } = &mut decision {
            if let Some(index) = events.iter().position(|event| event.event_type == "run.created") {
                events.insert(index + 1, event(&plan_thread, command_id, "plan.updated", plan, now));
            }
        }
    }
    Ok(decision)
}

/// `manualContinuationOfRunId`: the user's "continue" from an interrupted or usage-limited run
/// is only honoured while that run is still the thread's latest executed one.
fn check_manual_continuation(command: &Value, projection: &Value) -> Result<(), StoreError> {
    let Some(source_id) = command.get("manualContinuationOfRunId") else { return Ok(()) };
    let source = projection["runs"].as_array().unwrap().iter().find(|run| run["id"] == *source_id);
    let limited = source
        .and_then(|run| crate::message_queue::root_failure(projection, run))
        .is_some_and(|failure| failure["class"] == "usage_limit");
    let continuable = source.is_some_and(|run| run["status"] == "interrupted" || (run["status"] == "failed" && limited));
    let latest = crate::message_queue::latest_executed(projection).zip(source).is_some_and(|(latest, source)| latest["id"] == source["id"]);
    if command["dispatchMode"]["type"] != "start_immediately"
        || !continuable
        || !latest
        || !projection["thread"]["archivedAt"].is_null()
        || !projection["thread"]["deletedAt"].is_null()
        || projection["runtimeRequests"].as_array().unwrap().iter().any(|request| request["status"] == "pending")
    {
        return Err(StoreError::InvalidCommand("This thread can no longer be resumed from that run.".into()));
    }
    Ok(())
}

/// `sourcePlanRef`: the proposed plan this message implements, which completes with the run it
/// starts. Returns the thread that owns the plan and the plan as completed.
fn source_plan_completion(
    command: &Value,
    projection: &Value,
    transaction: Option<&rusqlite::Transaction<'_>>,
) -> Result<Option<(String, Value)>, StoreError> {
    let Some(reference) = command.get("sourcePlanRef") else { return Ok(None) };
    let (plan_thread, plan_id) = (reference["threadId"].as_str().unwrap_or(""), reference["planId"].as_str().unwrap_or(""));
    let missing = || StoreError::InvalidCommand(format!("Proposed plan {plan_id} does not exist on thread {plan_thread}."));
    let owner = if projection["thread"]["id"] == plan_thread {
        projection.clone()
    } else {
        let transaction = transaction.ok_or_else(missing)?;
        read_projection(transaction, "thread", plan_thread)?.ok_or_else(missing)?
    };
    let plan = owner["plans"].as_array().into_iter().flatten().find(|plan| plan["id"] == plan_id && plan["kind"] == "proposed_plan").ok_or_else(missing)?;
    if owner["thread"]["projectId"] != projection["thread"]["projectId"] {
        return Err(StoreError::InvalidCommand(format!("Proposed plan {plan_id} belongs to a different project.")));
    }
    if plan["status"] != "active" {
        return Err(StoreError::InvalidCommand(format!("Proposed plan {plan_id} is not active.")));
    }
    let mut completed = plan.clone();
    completed["status"] = json!("completed");
    Ok(Some((plan_thread.to_owned(), completed)))
}

fn plan_message_delivery(
    command: &Value,
    projection: &Value,
    now: DateTime<Utc>,
    driver: &str,
    reuse_message: bool,
    transaction: Option<&rusqlite::Transaction<'_>>,
) -> Result<Decision, StoreError> {
    let mut resolved = command.clone();
    resolved["dispatchMode"] = crate::message_steering::resolve_dispatch_mode(command, projection);
    if resolved["dispatchMode"]["type"] == "steer_active"
        && crate::message_steering::completed_steer_target(
            projection,
            &resolved["dispatchMode"]["targetRunId"],
        )
    {
        resolved["dispatchMode"] = json!({"type":"start_immediately"});
    }
    // A delegated-completion delivery is the server's own queued wake; its text names the tasks owed now.
    crate::delegated_completion::admit_message(&mut resolved, projection)?;
    let command = &resolved;
    // A usage-limit continuation is honoured only while the recovery that scheduled it is current.
    if let Some(decision) = crate::usage_limits::admit_continuation(command, projection, now)? {
        return Ok(decision);
    }
    // A restart continuation is honoured only while its source is the thread's latest cancelled run;
    // a stale delivery is accepted and changes nothing.
    if let Some(source_id) = command.get("restartContinuationOfRunId") {
        let source = projection["runs"].as_array().into_iter().flatten().find(|run| run["id"] == *source_id);
        if !source.is_some_and(|source| crate::restart_continuation::source_stands(projection, source)) {
            return crate::restart_continuation::stale(command, projection, now);
        }
    }
    if command.get("notification").is_some()
        && (command["createdBy"] != "agent"
            || !matches!(command["creationSource"].as_str(), Some("server" | "provider"))
            || command["dispatchMode"]["type"] != "queue_after_active")
    {
        return Err(StoreError::InvalidCommand("Notifications must be server- or provider-created queued messages.".into()));
    }
    let deferred = command["dispatchMode"]["type"] == "defer_start";
    if !matches!(
        command["dispatchMode"]["type"].as_str(),
        Some(
            "defer_start"
                | "start_immediately"
                | "queue_after_active"
                | "steer_active"
                | "restart_active"
        )
    ) {
        return Err(StoreError::InvalidCommand(
            "Unsupported message delivery mode.".into(),
        ));
    }
    // Attachments were claimed into the thread's store by the transport before this plan.
    let runs = projection["runs"].as_array().unwrap();
    if !reuse_message
        && projection["messages"]
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
    // A message to another provider than the thread's active provider thread runs there, with the
    // conversation so far handed over (`context_transfer.rs`). It starts its own run: it can neither
    // steer nor wait behind one that is still going.
    let instance_changed = model["instanceId"] != projection["thread"]["modelSelection"]["instanceId"];
    let provider_switch = projection["providerThreads"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == projection["thread"]["activeProviderThreadId"])
        .is_some_and(|row| row["providerInstanceId"] != model["instanceId"]);
    // Accounts that share a native conversation continue it: no handoff, the same provider thread.
    let resume_across = provider_switch && command["resumeAcrossInstances"] == true;
    let handoff_switch = provider_switch && !resume_across;
    // Steering a running turn onto another provider restarts it there, which is not ported.
    if (instance_changed || provider_switch) && matches!(command["dispatchMode"]["type"].as_str(), Some("steer_active" | "restart_active")) {
        return Err(StoreError::InvalidCommand(
            "A message to another provider cannot steer or restart a run that is still active.".into(),
        ));
    }
    if matches!(
        command["dispatchMode"]["type"].as_str(),
        Some("steer_active" | "restart_active")
    ) {
        return crate::message_steering::plan(command, projection, now, transaction);
    }
    if let Some(active) = runs
        .iter()
        .rev()
        .find(|run| crate::message_queue::blocking(run))
    {
        if projection["contextTransfers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|transfer| {
                transfer["type"] == "merge_back"
                    && transfer["targetThreadId"] == projection["thread"]["id"]
                    && transfer["status"] == "pending"
            })
        {
            return Err(StoreError::InvalidCommand(format!(
                "Thread {thread_id} has a pending merge-back transfer; queued merge-back consumption is not implemented yet."
            )));
        }
        let provider = projection["providerThreads"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == projection["thread"]["activeProviderThreadId"])
            .or_else(|| {
                projection["providerThreads"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|row| row["id"] == active["providerThreadId"])
            })
            .ok_or_else(|| {
                StoreError::InvalidCommand(format!(
                    "Active run {} has no provider thread for queued dispatch.",
                    active["id"].as_str().unwrap()
                ))
            })?;
        // A message to another provider waits on that provider's thread for this conversation.
        let (target, new_provider_thread) = crate::provider_switch::queue_provider_thread(projection, provider, model, driver, &uuid::Uuid::new_v4().to_string(), &timestamp);
        let session = projection["providerSessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|session| session["id"] == target["providerSessionId"]);
        let supported = supports_queued_messages(session, driver);
        if !supported {
            return Err(StoreError::InvalidCommand(format!(
                "Provider instance {} does not support queued messages.",
                model["instanceId"].as_str().unwrap()
            )));
        }
        let context = crate::message_queue::QueueContext {
            provider_thread: &target,
            new_provider_thread,
            checkpoint_scope: None,
        };
        return if reuse_message {
            crate::message_queue::allocate_steer_follow_up(command, projection, context, now)
        } else {
            crate::message_queue::allocate(command, projection, context, now)
        };
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
    let existing_provider_thread = if resume_across {
        projection["providerThreads"].as_array().unwrap().iter().find(|row| row["id"] == projection["thread"]["activeProviderThreadId"])
    } else if handoff_switch {
        // Switching back to a provider the thread already ran on continues that provider thread
        // (`rootProviderThreadsForProvider`: the thread's own, newest run first).
        projection["providerThreads"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["providerInstanceId"] == model["instanceId"] && row["appThreadId"] == projection["thread"]["id"] && row["ownerNodeId"].is_null() && row["status"] != "closed")
            .max_by(|left, right| left["lastRunOrdinal"].as_u64().cmp(&right["lastRunOrdinal"].as_u64()).then_with(|| left["updatedAt"].as_str().cmp(&right["updatedAt"].as_str())))
    } else {
        projection["providerThreads"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| {
                row["id"] == projection["thread"]["activeProviderThreadId"]
                    && row["providerInstanceId"] == model["instanceId"]
                    && row["status"] != "closed"
            })
    };
    let provider_thread_id = existing_provider_thread
        .and_then(|row| row["id"].as_str())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let mut events = vec![];
    if !crate::message_queue::model_selections_equal(model, &projection["thread"]["modelSelection"])
    {
        if !instance_changed {
            if let crate::provider_selection_transition::SelectionTransition::Reject { reason } =
                crate::provider_selection_transition::classify_selection(projection, model)
            {
                return Err(StoreError::InvalidCommand(reason));
            }
        }
        let mut thread = projection["thread"].clone();
        thread["modelSelection"] = model.clone();
        thread["providerInstanceId"] = model["instanceId"].clone();
        thread["updatedAt"] = json!(timestamp);
        events.push(event(
            thread_id,
            command_id,
            if instance_changed { "thread.provider-switched" } else { "thread.model-selection-updated" },
            thread,
            now,
        ));
    }
    // Only continuing the native thread under another account releases the session it ran in; a handoff
    // leaves the old provider's session to its idle release (`dispatchMessage`).
    let mut released = vec![];
    if provider_switch && resume_across {
        let detail = if resume_across { "Provider account changed; continuing the native thread." } else { "Provider switched." };
        // Continuing the native thread releases only the session it ran in; a switch releases every other provider's.
        let active_session = existing_provider_thread.filter(|_| resume_across).map(|row| row["providerSessionId"].clone());
        for session in projection["providerSessions"].as_array().unwrap().iter().filter(|session| !matches!(session["status"].as_str(), Some("stopped" | "error")) && match &active_session { Some(id) => session["id"] == *id, None => session["providerInstanceId"] != model["instanceId"] }) {
            events.push(Event {
                event_id: uuid::Uuid::new_v4().to_string(),
                aggregate_kind: "thread".into(), aggregate_id: thread_id.into(), occurred_at: timestamp.clone(),
                command_id: Some(command_id.into()), causation_event_id: None,
                correlation_id: Some(command_id.into()), event_type: "provider-session.detached".into(),
                payload: json!({"providerSessionId":session["id"],"detachedAt":timestamp,"reason":detail}),
                metadata: json!({}),
            });
            released.push(NewEffect {
                id: format!("effect:{command_id}:provider-session.detach:{}", session["id"].as_str().unwrap_or("")),
                command_id: command_id.into(),
                thread_id: thread_id.into(),
                request: json!({"type":"provider-session.detach","providerSessionId":session["id"],"detail":detail}),
                available_at: timestamp.clone(),
            });
        }
    }
    // A fork's first run and the first run after a merge-back carry the transfer they wait on.
    let next_item_ordinal = projection["turnItems"].as_array().unwrap().iter().filter_map(|item| item["ordinal"].as_u64()).max().map_or(0, |ordinal| ordinal + 1);
    let transfer = crate::context_transfer::resolve(&crate::context_transfer::Planning {
        switch: handoff_switch.then_some(crate::context_transfer::Switch { target_provider_thread: existing_provider_thread }),
        command,
        projection,
        transaction,
        driver,
        model,
        run_id: &run_id,
        node_id: &node_id,
        provider_thread_id: &provider_thread_id,
        item_ordinal: next_item_ordinal,
        now,
    })?;
    if existing_provider_thread.is_none() {
        let provider_thread = checked::<t3_contracts::ProviderThread>(
            json!({"id":provider_thread_id,"driver":driver,"providerInstanceId":model["instanceId"],"providerSessionId":null,"appThreadId":thread_id,"ownerNodeId":null,"nativeThreadRef":null,"nativeConversationHeadRef":null,"status":"not_loaded","firstRunOrdinal":ordinal,"lastRunOrdinal":ordinal,"handoffIds":transfer.as_ref().map_or(vec![], |transfer| transfer.handoff_ids.clone()),"forkedFrom":transfer.as_ref().map_or(Value::Null, |transfer| transfer.forked_from.clone()),"pendingBackgroundTasks":[],"createdAt":timestamp,"updatedAt":timestamp}),
        )?;
        events.push(event(
            thread_id,
            command_id,
            "provider-thread.updated",
            provider_thread,
            now,
        ));
    } else if let Some(handoffs) = transfer.as_ref().map(|transfer| &transfer.handoff_ids).filter(|ids| !ids.is_empty()) {
        // The thread that takes a merge-back now owns its handoff.
        let mut provider_thread = existing_provider_thread.unwrap().clone();
        let mut ids: Vec<Value> = provider_thread["handoffIds"].as_array().cloned().unwrap_or_default();
        ids.extend(handoffs.iter().filter(|id| !ids.contains(id)).cloned().collect::<Vec<_>>());
        provider_thread["handoffIds"] = json!(ids);
        provider_thread["updatedAt"] = json!(timestamp);
        events.push(event(thread_id, command_id, "provider-thread.updated", checked::<t3_contracts::ProviderThread>(provider_thread)?, now));
    }
    if let Some(transfer) = &transfer {
        for (kind, payload) in &transfer.early {
            events.push(event(thread_id, command_id, kind, checked_transfer_row(kind, payload.clone())?, now));
        }
    }
    let mut run = json!({"id":run_id,"threadId":thread_id,"ordinal":ordinal,"providerInstanceId":model["instanceId"],"modelSelection":model,"providerThreadId":provider_thread_id,"userMessageId":command["messageId"],"rootNodeId":node_id,"activeAttemptId":attempt_id,"status":if deferred{"preparing"}else{"starting"},"requestedAt":timestamp,"startedAt":null,"completedAt":null,"checkpointId":null,"contextHandoffId":transfer.as_ref().map_or(Value::Null, |transfer| transfer.run_handoff_id.clone())});
    if deferred {
        if let Some(strategy) = command["dispatchMode"].get("workspaceStrategy") {
            run["workspacePreparation"] = strategy.clone();
        }
    }
    if let Some(plan) = command.get("sourcePlanRef") {
        run["sourcePlanRef"] = plan.clone();
    }
    if let Some(source) = command.get("restartContinuationOfRunId") {
        run["restartContinuationOfRunId"] = source.clone();
    }
    let run = checked::<t3_contracts::Run>(run)?;
    let attempt = checked::<t3_contracts::RunAttempt>(
        json!({"id":attempt_id,"runId":run_id,"attemptOrdinal":1,"rootNodeId":node_id,"providerInstanceId":model["instanceId"],"providerThreadId":provider_thread_id,"providerTurnId":null,"reason":"initial","status":"pending","startedAt":null,"completedAt":null}),
    )?;
    // A deferred run gets its scope when the prepared workspace is released.
    let scope = if deferred {
        None
    } else {
        let fallback = std::env::current_dir().ok();
        Some(crate::message_steering::prepare_root_scope(
            projection,
            &json!(run_id),
            &json!(node_id),
            &json!(provider_thread_id),
            fallback.as_deref().and_then(|path| path.to_str()),
            now,
            transaction,
        )?)
    };
    let node = checked::<t3_contracts::ExecutionNode>(
        json!({"id":node_id,"threadId":thread_id,"runId":run_id,"parentNodeId":null,"rootNodeId":node_id,"kind":"root_turn","status":"pending","countsForRun":true,"providerThreadId":provider_thread_id,"providerTurnId":null,"nativeItemRef":null,"runtimeRequestId":null,"checkpointScopeId":scope.as_ref().map(|scope|scope["id"].clone()),"startedAt":null,"completedAt":null}),
    )?;
    let mut message = json!({"createdBy":command["createdBy"],"creationSource":command["creationSource"],"id":command["messageId"],"threadId":thread_id,"runId":run_id,"nodeId":node_id,"role":"user","text":command["text"],"attachments":command["attachments"],"streaming":false,"createdAt":timestamp,"updatedAt":timestamp});
    let mut item = json!({"id":user_item_id(command["messageId"].as_str().unwrap()),"threadId":thread_id,"runId":run_id,"nodeId":node_id,"providerThreadId":provider_thread_id,"providerTurnId":null,"nativeItemRef":null,"parentItemId":null,"ordinal":next_item_ordinal + transfer.as_ref().map_or(0, |transfer| u64::from(transfer.handoff_item.is_some())),"status":"completed","title":null,"startedAt":timestamp,"completedAt":timestamp,"updatedAt":timestamp,"type":"user_message","createdBy":command["createdBy"],"creationSource":command["creationSource"],"messageId":command["messageId"],"inputIntent":"turn_start","text":command["text"],"attachments":command["attachments"]});
    if reuse_message {
        if let Some(existing) = projection["turnItems"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| {
                item["type"] == "user_message" && item["messageId"] == command["messageId"]
            })
        {
            item["id"] = existing["id"].clone();
        }
    }
    // Who sent the message and which scheduled task it came from, for the transcript and
    // the provider's view of the turn.
    for field in ["senderThreadId", "scheduledTaskId", "context"] {
        if let Some(value) = command.get(field) {
            message[field] = value.clone();
            item[field] = value.clone();
        }
    }
    if let Some(notification) = command.get("notification") {
        message["notification"] = notification.clone();
    }
    // A message that reports background work is shown as that report, not as text the user wrote.
    let item = crate::notification::notification_turn_item(item, &message);
    let message = checked::<t3_contracts::ConversationMessage>(message)?;
    let item = checked::<t3_contracts::TurnItem>(item)?;
    let handoff_item = transfer.as_ref().and_then(|transfer| transfer.handoff_item.clone()).map(checked::<t3_contracts::TurnItem>).transpose()?;
    for (kind, payload) in [
        ("run.created", run),
        ("run-attempt.created", attempt),
        ("node.updated", node),
    ]
    .into_iter()
    .chain(scope.map(|scope| ("checkpoint-scope.created", scope)))
    .chain(handoff_item.map(|item| ("turn-item.updated", item)))
    .chain([("message.updated", message), ("turn-item.updated", item)])
    {
        events.push(event(thread_id, command_id, kind, payload, now));
    }
    if let Some(transfer) = &transfer {
        for (kind, payload) in &transfer.late {
            events.push(event(thread_id, command_id, kind, checked_transfer_row(kind, payload.clone())?, now));
        }
    }
    if deferred {
        let item = checked::<t3_contracts::TurnItem>(
            json!({"id":format!("workspace-preparation:{run_id}"),"threadId":thread_id,"runId":run_id,"nodeId":node_id,"providerThreadId":provider_thread_id,"providerTurnId":null,"nativeItemRef":null,"parentItemId":null,"ordinal":projection["turnItems"].as_array().unwrap().len()+1,"status":"running","title":"Preparing workspace","startedAt":timestamp,"completedAt":null,"updatedAt":timestamp,"type":"command_execution","input":"Preparing workspace"}),
        )?;
        events.push(event(thread_id, command_id, "turn-item.updated", item, now));
    }
    // A worktree is created after this commit, by an effect; its release starts the turn.
    let worktree = deferred && command["dispatchMode"]["workspaceStrategy"]["type"] == "worktree";
    let mut effects = released;
    effects.extend(if worktree {
            vec![NewEffect {
                id: format!("effect:{command_id}:workspace.prepare:{run_id}"),
                command_id: command_id.into(),
                thread_id: thread_id.into(),
                request: json!({"type":"workspace.prepare","baseCommandId":command_id,"runId":run_id,"workspaceStrategy":command["dispatchMode"]["workspaceStrategy"]}),
                available_at: timestamp,
            }]
        } else if deferred {
            vec![]
        } else {
            vec![NewEffect {
                id: format!("effect:{command_id}:provider-turn.start:{run_id}"),
                command_id: command_id.into(),
                thread_id: thread_id.into(),
                request: json!({"type":"provider-turn.start","runId":run_id}),
                available_at: timestamp,
            }]
        });
    Ok(Decision::Accepted { events, effects })
}

/// Only actor delivery calls this after its recorded native steering turn has
/// completed. The persisted effect, message and target turn are checked inside
/// the same planning transaction; no public command admits message reuse.
pub(crate) fn follow_up_completed_steer(
    store: &Store,
    effect: &crate::persistence::Effect,
) -> Result<Receipt, StoreError> {
    let command_id = format!("command:steer-follow-up:{}", effect.id);
    let now = Utc::now();
    let (receipt, _) = store.dispatch_resolved(
        &command_id, "thread", "message.dispatch", now,
        |transaction, _| {
            let persisted: (String, String, String) = transaction.query_row(
                "SELECT command_id,thread_id,request_json FROM rust_effect_outbox WHERE id=?1",
                [&effect.id], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
            )?;
            let request: Value = serde_json::from_str(&persisted.2)?;
            if persisted.0 != effect.command_id || persisted.1 != effect.thread_id || request != effect.request || request["type"] != "provider-turn.steer" {
                return Err(StoreError::InvalidCommand("Steering follow-up does not match its durable effect.".into()));
            }
            Ok(effect.thread_id.clone())
        },
        |transaction, _| {
            let projection = read_projection(transaction,"thread",&effect.thread_id)?.ok_or_else(||StoreError::InvalidCommand("Thread not found.".into()))?;
            if !projection["thread"]["deletedAt"].is_null() || !projection["thread"]["archivedAt"].is_null() {
                return Err(StoreError::InvalidCommand("Thread is deleted or archived.".into()));
            }
            let find = |field: &str, id: &Value| projection[field].as_array().unwrap().iter().find(|row|row["id"] == *id)
                .ok_or_else(||StoreError::InvalidCommand(format!("Recorded steering {field} identity is missing.")));
            let message=find("messages",&effect.request["messageId"])?;
            let run=find("runs",&message["runId"])?;
            let turn=find("providerTurns",&effect.request["providerTurnId"])?;
            let provider=find("providerThreads",&effect.request["providerThreadId"])?;
            let attempt=find("attempts",&turn["runAttemptId"])?;
            if turn["status"] != "completed" || turn["providerThreadId"] != provider["id"] || attempt["runId"] != run["id"] || provider["providerSessionId"] != effect.request["providerSessionId"] {
                return Err(StoreError::InvalidCommand("Recorded steering target has not completed or changed session.".into()));
            }
            let mut command=json!({"type":"message.dispatch","commandId":command_id,"threadId":effect.thread_id,"messageId":message["id"],"text":message["text"],"attachments":message["attachments"],"createdBy":message["createdBy"],"creationSource":message["creationSource"],"dispatchMode":{"type":if message.get("delegatedCompletion").is_some(){"queue_after_active"}else{"start_immediately"}}});
            for field in ["senderThreadId","context","delegatedCompletion","notification"] {
                if let Some(value)=message.get(field) {command[field]=value.clone()}
            }
            if message.get("delegatedCompletion").is_some() {command["modelSelection"]=run["modelSelection"].clone()}
            plan_message_inner(&command,&projection,now,provider["driver"].as_str().unwrap(),true,Some(transaction))
        },
        thread::reduce,
    )?;
    Ok(receipt)
}

pub(crate) fn plan_release(
    command: &Value,
    projection: &Value,
    now: DateTime<Utc>,
) -> Result<Decision, StoreError> {
    plan_release_in(command, projection, now, None)
}
pub(crate) fn plan_release_in(
    command: &Value,
    projection: &Value,
    now: DateTime<Utc>,
    transaction: Option<&rusqlite::Transaction<'_>>,
) -> Result<Decision, StoreError> {
    let run = projection["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|run| run["id"] == command["runId"] && run["status"] == "preparing")
        .ok_or_else(|| {
            StoreError::InvalidCommand("Run is not awaiting workspace preparation.".into())
        })?;
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
    // The workspace is now known: the run's root checkpoint scope is created with it.
    let fallback = std::env::current_dir().ok();
    let scope = crate::message_steering::prepare_root_scope(
        projection,
        &run["id"],
        &run["rootNodeId"],
        &run["providerThreadId"],
        fallback.as_deref().and_then(|path| path.to_str()),
        now,
        transaction,
    )?;
    let mut root = projection["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["id"] == run["rootNodeId"])
        .cloned()
        .ok_or_else(|| StoreError::InvalidCommand("Prepared run has no root node.".into()))?;
    root["checkpointScopeId"] = scope["id"].clone();
    Ok(Decision::Accepted {
        events: vec![
            event(thread_id, command_id, "checkpoint-scope.created", scope, now),
            event(
                thread_id,
                command_id,
                "node.updated",
                checked::<t3_contracts::ExecutionNode>(root)?,
                now,
            ),
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

    #[test]
    fn a_message_implementing_a_plan_completes_that_active_plan_only() {
        let plan = |status: &str| json!({"id":"plan:1","threadId":"thread","kind":"proposed_plan","status":status});
        let projection = |status: &str, project: &str| json!({"thread":{"id":"thread","projectId":project},"plans":[plan(status)]});
        let reference = json!({"type":"message.dispatch","sourcePlanRef":{"threadId":"thread","planId":"plan:1"}});
        let (owner, completed) = source_plan_completion(&reference, &projection("active", "project"), None).unwrap().unwrap();
        assert_eq!((owner.as_str(), completed["status"].as_str()), ("thread", Some("completed")));
        assert!(source_plan_completion(&json!({"type":"message.dispatch"}), &projection("active", "project"), None).unwrap().is_none());
        let refused = |projection: &Value, reference: &Value| source_plan_completion(reference, projection, None).unwrap_err().to_string();
        assert_eq!(refused(&projection("completed", "project"), &reference), "Invalid command: Proposed plan plan:1 is not active.");
        let missing = json!({"sourcePlanRef":{"threadId":"thread","planId":"plan:2"}});
        assert_eq!(refused(&projection("active", "project"), &missing), "Invalid command: Proposed plan plan:2 does not exist on thread thread.");
        // A plan on another thread needs that thread's projection, which only a transaction can read.
        let elsewhere = json!({"sourcePlanRef":{"threadId":"other","planId":"plan:1"}});
        assert_eq!(refused(&projection("active", "project"), &elsewhere), "Invalid command: Proposed plan plan:1 does not exist on thread other.");
    }
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
    fn active_turn_queues_each_new_turn_mode_without_early_transcript_or_effect() {
        for dispatch in [
            json!({"type":"start_immediately"}),
            json!({"type":"queue_after_active"}),
            json!({"type":"defer_start","workspaceStrategy":{"type":"worktree"}}),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let store = Store::memory().unwrap();
            project(&store, directory.path());
            let id = launch(&store, "launch");
            let mut first = message(&id, "first", "First");
            first["dispatchMode"] =
                json!({"type":"defer_start","workspaceStrategy":{"type":"root"}});
            let dispatch_command = |command: &Value| {
                store
                    .dispatch(
                        command["commandId"].as_str().unwrap(),
                        "thread",
                        &id,
                        "message.dispatch",
                        Utc::now(),
                        |transaction| {
                            plan_message(
                                command,
                                &read_projection(transaction, "thread", &id)?.unwrap(),
                                Utc::now(),
                            )
                        },
                        thread::reduce,
                    )
                    .unwrap()
            };
            dispatch_command(&first);
            let mut next = message(&id, "second", "Next");
            next["dispatchMode"] = dispatch;
            next["senderThreadId"] = json!("trusted-sender");
            let receipt = dispatch_command(&next);
            assert_eq!(dispatch_command(&next), receipt);
            let projection = store.projection("thread", &id).unwrap().unwrap();
            assert_eq!(projection["runs"][0]["status"], "preparing");
            assert_eq!(projection["runs"][1]["status"], "queued");
            assert_eq!(
                projection["messages"][1]["senderThreadId"],
                "trusted-sender"
            );
            assert!(
                !projection["turnItems"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|item| item["messageId"] == next["messageId"])
            );
            assert!(
                store
                    .claim_effect("worker", Utc::now(), chrono::Duration::minutes(1))
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn cold_acp_queue_uses_adapter_defaults_and_negotiated_refusal_wins() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::memory().unwrap();
        project(&store, directory.path());
        let id = launch(&store, "launch");
        let mut first = message(&id, "first", "First");
        first["dispatchMode"] = json!({"type":"defer_start","workspaceStrategy":{"type":"root"}});
        store
            .dispatch(
                "first",
                "thread",
                &id,
                "message.dispatch",
                Utc::now(),
                |transaction| {
                    plan_message(
                        &first,
                        &read_projection(transaction, "thread", &id)?.unwrap(),
                        Utc::now(),
                    )
                },
                thread::reduce,
            )
            .unwrap();
        let mut projection = store.projection("thread", &id).unwrap().unwrap();
        projection["providerThreads"][0]["driver"] = json!("acpRegistry");
        assert!(
            projection["providerSessions"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let mut next = message(&id, "second", "Next");
        next["dispatchMode"] = json!({"type":"queue_after_active"});
        for driver in ["acpRegistry", "antigravity"] {
            let Decision::Accepted { events, effects } =
                plan_message_for_driver(&next, &projection, Utc::now(), driver).unwrap()
            else {
                panic!("cold ACP queue rejected")
            };
            assert!(effects.is_empty());
            assert!(
                events.iter().any(|event| event.event_type == "run.created"
                    && event.payload["status"] == "queued")
            );
            assert!(
                !events
                    .iter()
                    .any(|event| event.event_type == "turn-item.updated")
            );
        }
        projection["providerThreads"][0]["providerSessionId"] = json!("session:negotiated");
        projection["providerSessions"] = json!([{"id":"session:negotiated","capabilities":{"turns":{"supportsQueuedMessages":false}}}]);
        assert!(
            matches!(plan_message_for_driver(&next, &projection, Utc::now(), "acpRegistry"), Err(StoreError::InvalidCommand(message)) if message.contains("does not support queued messages"))
        );
        assert!(!supports_queued_messages(None, "not-registered"));
    }

    #[test]
    fn idle_queue_mode_starts_normally_and_preserves_trusted_sender() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::memory().unwrap();
        project(&store, directory.path());
        let id = launch(&store, "launch");
        let mut command = message(&id, "first", "First");
        command["dispatchMode"] = json!({"type":"queue_after_active"});
        command["senderThreadId"] = json!("trusted-sender");
        store
            .dispatch(
                "first",
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
        assert_eq!(projection["runs"][0]["status"], "starting");
        assert_eq!(
            projection["messages"][0]["senderThreadId"],
            "trusted-sender"
        );
        assert_eq!(
            projection["turnItems"][0]["senderThreadId"],
            "trusted-sender"
        );
        assert_eq!(projection["turnItems"][0]["inputIntent"], "turn_start");
        assert!(
            store
                .claim_effect("worker", Utc::now(), chrono::Duration::minutes(1))
                .unwrap()
                .is_some()
        );
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
            json!({"providerInstances":{"codex":{"driver":"codex","enabled":false},"claudeAgent":{"driver":"claudeAgent","enabled":false}}}),
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
        let settings:t3_contracts::ServerSettings=serde_json::from_value(json!({"providerInstances":{"claudeAgent":{"driver":"claudeAgent","enabled":false},"codex":{"driver":"codex","config":{"binaryPath":binary,"homePath":directory.path().join("codex-home")},"environment":[{"name":"FIXTURE_REQUEST_SIGNAL","value":signal_path}]}}})).unwrap();
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
        let settings:t3_contracts::ServerSettings=serde_json::from_value(json!({"providerInstances":{"claudeAgent":{"driver":"claudeAgent","enabled":false},"codex":{"driver":"codex","config":{"binaryPath":binary,"homePath":directory.path().join("codex-home")},"environment":[{"name":"FIXTURE_NUMERIC_REQUEST_IDS","value":"1"}]}}})).unwrap();
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
