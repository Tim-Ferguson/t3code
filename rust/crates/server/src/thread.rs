use crate::persistence::{
    Decision, Event, NewEffect, Store, StoreError, StoredEvent, read_projection, read_projections,
    write_projection,
};
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::Transaction;
use serde_json::{Value, json};

#[derive(Clone)]
pub struct ThreadService {
    store: Store,
}

impl ThreadService {
    pub fn new(store: Store) -> Self {
        Self { store }
    }

    pub fn dispatch(
        &self,
        command: &Value,
        now: DateTime<Utc>,
    ) -> Result<crate::persistence::Receipt, StoreError> {
        self.dispatch_guarded(command, now, |_, _| Ok(()))
    }
    pub fn dispatch_guarded(
        &self,
        command: &Value,
        now: DateTime<Utc>,
        authorize: impl Fn(&Transaction<'_>, Option<&Value>) -> Result<(), StoreError>,
    ) -> Result<crate::persistence::Receipt, StoreError> {
        let command = normalize_command(command).map_err(StoreError::InvalidCommand)?;
        let command = &command;
        let command_id = string(command, "commandId").unwrap_or("");
        let thread_id = string(command, "threadId").unwrap_or("");
        let kind = string(command, "type").unwrap_or("");
        self.store.dispatch(
            command_id,
            "thread",
            thread_id,
            kind,
            now,
            |transaction| {
                let current = read_projection(transaction, "thread", thread_id)?;
                authorize(transaction, current.as_ref())?;
                let project = string(command, "projectId")
                    .map(|id| read_projection(transaction, "project", id))
                    .transpose()?
                    .flatten();
                Ok(
                    match plan(command, current.as_ref(), project.as_ref(), now) {
                        Ok(events) => {
                            let effects = events.iter().filter(|event| event.event_type == "provider-session.detached").map(|event| NewEffect {
                                id: format!("effect:{command_id}:provider-session.detach:{}", event.payload["providerSessionId"].as_str().unwrap()),
                                command_id: command_id.into(),
                                thread_id: thread_id.into(),
                                request: json!({"type":"provider-session.detach","providerSessionId":event.payload["providerSessionId"],"detail":event.payload["reason"]}),
                                available_at: iso(now),
                            }).collect();
                            Decision::Accepted { events, effects }
                        },
                        Err(error) => Decision::Rejected(error),
                    },
                )
            },
            reduce,
        )
    }

    pub fn projection(&self, id: &str) -> Result<Option<Value>, StoreError> {
        self.store.projection("thread", id)
    }

    pub fn shell_snapshot(&self) -> Result<Value, StoreError> {
        self.store.read(|connection| {
            let sequence: u64 = connection.query_row("SELECT COALESCE(MAX(sequence),0) FROM rust_application_events",[],|row| row.get(0))?;
            let projects = read_projections(connection,"project")?.into_iter().filter(|row| row["deletedAt"].is_null()).map(crate::project::to_shell).collect::<Vec<_>>();
            let rows = read_projections(connection,"thread")?;
            let threads = rows.iter().filter(|projection| projection["thread"]["deletedAt"].is_null() && projection["thread"]["archivedAt"].is_null()).map(shell).collect::<Vec<_>>();
            let archived = rows.iter().filter(|projection| projection["thread"]["deletedAt"].is_null() && !projection["thread"]["archivedAt"].is_null()).map(shell).collect::<Vec<_>>();
            Ok(json!({"schemaVersion":2,"snapshotSequence":sequence,"projects":projects,"threads":threads,"archivedThreads":archived}))
        })
    }
}

fn string<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    value.get(field).and_then(Value::as_str)
}
fn array<'a>(value: &'a Value, field: &str) -> &'a [Value] {
    value
        .get(field)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}
fn time(value: &Value) -> Option<DateTime<Utc>> {
    value.as_str().and_then(|value| value.parse().ok())
}
fn iso(now: DateTime<Utc>) -> String {
    now.to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub fn normalize_command(input: &Value) -> Result<Value, String> {
    let mut input = input.clone();
    if !input.is_object() {
        return Err("command must be an object".into());
    }
    if input["type"] == "thread.create" {
        if input.get("createdBy").is_none() {
            input["createdBy"] = json!("user");
        }
        if input.get("creationSource").is_none() {
            input["creationSource"] = json!("web");
        }
    }
    let decoded: t3_contracts::ThreadCommand =
        serde_json::from_value(input).map_err(|error| error.to_string())?;
    decoded.service_payload().map_err(|error| error.to_string())
}

/// Lifecycle planning is pure and executes under the same transaction as its receipt.
pub fn plan(
    command: &Value,
    projection: Option<&Value>,
    project: Option<&Value>,
    now: DateTime<Utc>,
) -> Result<Vec<Event>, Value> {
    let normalized=normalize_command(command).map_err(|detail|json!({"_tag":"OrchestratorDispatchError","commandId":command["commandId"],"commandType":command["type"],"cause":detail}))?;
    let command = &normalized;
    let kind = string(command, "type").unwrap_or("");
    let id = string(command, "threadId").unwrap_or("");
    let command_id = string(command, "commandId").unwrap_or("");
    let reject = |detail: String| json!({"_tag":"OrchestratorDispatchError","commandId":command_id,"commandType":kind,"cause":detail});
    let unsupported = |detail: &str| reject(format!("Native backend has not yet ported {detail}."));
    let required = |field: &str| {
        string(command, field)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| reject(format!("{field} must be a non-empty string.")))
    };
    required("commandId")?;
    required("threadId")?;
    let at = iso(now);
    let (event_type, mut thread) = if kind == "thread.create" {
        if projection.is_some() {
            return Err(reject(format!("Thread {id} already exists.")));
        }
        if project
            .filter(|project| project["deletedAt"].is_null())
            .is_none()
        {
            return Err(reject(format!(
                "Project {} does not exist.",
                required("projectId")?
            )));
        }
        if command.get("importedNativeThread").is_some() {
            return Err(unsupported("native thread import"));
        }
        serde_json::from_value::<t3_contracts::ModelSelection>(command["modelSelection"].clone())
            .map_err(|error| reject(error.to_string()))?;
        serde_json::from_value::<t3_contracts::RuntimeMode>(command["runtimeMode"].clone())
            .map_err(|error| reject(error.to_string()))?;
        serde_json::from_value::<t3_contracts::ProviderInteractionMode>(
            command["interactionMode"].clone(),
        )
        .map_err(|error| reject(error.to_string()))?;
        (
            "thread.created",
            json!({
                "createdBy":command.get("createdBy").cloned().unwrap_or(json!("user")),
                "creationSource":command.get("creationSource").cloned().unwrap_or(json!("web")),
                "id":id,"projectId":required("projectId")?,"title":required("title")?,
                "providerInstanceId":command["modelSelection"]["instanceId"],"modelSelection":command["modelSelection"],
                "runtimeMode":command["runtimeMode"],"interactionMode":command["interactionMode"],
                "branch":command.get("branch").cloned().unwrap_or(Value::Null),"worktreePath":command.get("worktreePath").cloned().unwrap_or(Value::Null),
                "activeProviderThreadId":null,"lineage":{"parentThreadId":null,"relationshipToParent":null,"rootThreadId":id},
                "forkedFrom":null,"createdAt":at,"updatedAt":at,"archivedAt":null,"settledOverride":null,"settledAt":null,
                "snoozedUntil":null,"snoozedAt":null,"lastVisitedAt":null,"deletedAt":null
            }),
        )
    } else {
        let projection =
            projection.ok_or_else(|| reject(format!("Thread {id} does not exist.")))?;
        let mut thread = projection["thread"].clone();
        if !thread["deletedAt"].is_null() {
            return Err(reject(format!("Thread {id} is deleted.")));
        }
        if matches!(
            kind,
            "thread.settle"
                | "thread.unsettle"
                | "thread.snooze"
                | "thread.unsnooze"
                | "thread.auto-settle.set"
                | "thread.pin"
                | "thread.unpin"
                | "thread.pin.reorder"
                | "thread.active.reorder"
        ) && !thread["archivedAt"].is_null()
        {
            return Err(reject(format!("Thread {id} is archived.")));
        }
        // Until provider teardown and delegated completion orchestration are ported,
        // operations needing those effects fail explicitly instead of dropping work.
        if matches!(kind, "thread.archive" | "thread.delete" | "thread.settle")
            && (!array(projection, "providerSessions").is_empty()
                || !array(projection, "subagents").is_empty())
        {
            return Err(unsupported("provider and delegated-task teardown"));
        }
        let old_updated_at = thread["updatedAt"].clone();
        thread["updatedAt"] = json!(at);
        let event_type = match kind {
            "thread.archive" => {
                if !thread["archivedAt"].is_null() {
                    return Err(reject(format!("Thread {id} is already archived.")));
                }
                if !array(projection, "runs").is_empty() {
                    return Err(unsupported("archive run cancellation"));
                }
                thread["archivedAt"] = json!(at);
                thread["titleRegeneration"] = Value::Null;
                "thread.archived"
            }
            "thread.unarchive" => {
                if thread["archivedAt"].is_null() {
                    return Err(reject(format!("Thread {id} is not archived.")));
                }
                thread["archivedAt"] = Value::Null;
                "thread.unarchived"
            }
            "thread.delete" => {
                if !array(projection, "runs").is_empty()
                    || !array(projection, "runtimeRequests").is_empty()
                    || !array(projection, "messages").is_empty()
                {
                    return Err(unsupported("thread run and attachment cleanup"));
                }
                thread["deletedAt"] = json!(at);
                thread["titleRegeneration"] = Value::Null;
                "thread.deleted"
            }
            "thread.visit" => {
                let visited = time(&command["visitedAt"]).ok_or_else(|| {
                    reject(format!(
                        "Thread {id} visit time {} is not a valid timestamp.",
                        command["visitedAt"]
                    ))
                })?;
                if time(&thread["lastVisitedAt"]).is_none_or(|old| visited > old) {
                    thread["lastVisitedAt"] = json!(iso(visited));
                }
                thread["updatedAt"] = old_updated_at;
                "thread.visited"
            }
            "thread.mark-unread" => {
                let completed = array(projection, "runs")
                    .last()
                    .and_then(|run| time(&run["completedAt"]))
                    .ok_or_else(|| {
                        reject(format!("Thread {id} has no completed run to mark unread."))
                    })?;
                thread["lastVisitedAt"] = json!(iso(completed - chrono::Duration::milliseconds(1)));
                thread["updatedAt"] = old_updated_at;
                "thread.marked-unread"
            }
            "thread.settle" => {
                let automatic_ids = array(projection, "messages")
                    .iter()
                    .filter(|message| {
                        message.get("notification").is_some()
                            || message.get("delegatedCompletion").is_some()
                    })
                    .map(|message| message["id"].clone())
                    .collect::<Vec<_>>();
                let automatic = array(projection, "runs")
                    .iter()
                    .filter(|run| {
                        run["status"] == "queued" && automatic_ids.contains(&run["userMessageId"])
                    })
                    .collect::<Vec<_>>();
                let active = array(projection, "runs").iter().any(|run| {
                    matches!(
                        string(run, "status"),
                        Some("preparing" | "queued" | "starting" | "running" | "waiting")
                    ) && !automatic.contains(&run)
                });
                let requests = array(projection, "runtimeRequests")
                    .iter()
                    .filter(|request| request["status"] == "pending")
                    .collect::<Vec<_>>();
                let blocked = requests.iter().any(|request| {
                    request["kind"] != "user_input"
                        || request["responseCapability"]["type"] != "message"
                });
                if active || blocked {
                    return Err(reject(format!(
                        "Thread {id} has active or blocked work and cannot be settled."
                    )));
                }
                if !automatic.is_empty() || !requests.is_empty() {
                    return Err(unsupported("settle automatic delivery cancellation"));
                }
                let unchanged = thread["settledOverride"] == "settled"
                    && !thread["settledAt"].is_null()
                    && thread["pinnedAt"].is_null();
                thread["settledOverride"] = json!("settled");
                if !unchanged {
                    thread["settledAt"] = command.get("settledAt").cloned().unwrap_or(json!(at));
                }
                for field in ["unsettledAt", "pinnedAt", "pinOrderKey", "activeOrderKey"] {
                    thread[field] = Value::Null;
                }
                if unchanged {
                    thread["updatedAt"] = old_updated_at;
                }
                "thread.settled"
            }
            "thread.unsettle" => {
                let unchanged = thread["settledOverride"] == "active";
                thread["settledOverride"] = json!("active");
                thread["settledAt"] = Value::Null;
                if unchanged {
                    thread["updatedAt"] = old_updated_at;
                } else {
                    thread["unsettledAt"] = json!(at);
                }
                "thread.unsettled"
            }
            "thread.snooze" => {
                let wake = time(&command["snoozedUntil"])
                    .filter(|wake| *wake > now)
                    .ok_or_else(|| {
                        reject(format!(
                            "Thread {id} snooze wake time {} is not in the future.",
                            command["snoozedUntil"]
                        ))
                    })?;
                if array(projection, "runtimeRequests")
                    .iter()
                    .any(|request| request["status"] == "pending")
                {
                    return Err(reject(format!(
                        "Thread {id} has a pending approval or user-input request and cannot be snoozed."
                    )));
                }
                if array(projection, "runs")
                    .iter()
                    .any(|run| run["status"] == "queued")
                {
                    return Err(reject(format!(
                        "Thread {id} has a queued run and cannot be snoozed."
                    )));
                }
                let unchanged =
                    time(&thread["snoozedUntil"]) == Some(wake) && !thread["snoozedAt"].is_null();
                thread["snoozedUntil"] = json!(iso(wake));
                if unchanged {
                    thread["updatedAt"] = old_updated_at;
                } else {
                    thread["snoozedAt"] = json!(at);
                }
                if thread["limitRecovery"].is_object() {
                    thread["limitRecovery"]["snooze"] = json!(false);
                }
                "thread.snoozed"
            }
            "thread.unsnooze" => {
                if thread["snoozedUntil"].is_null() {
                    thread["updatedAt"] = old_updated_at;
                }
                thread["snoozedUntil"] = Value::Null;
                thread["snoozedAt"] = Value::Null;
                "thread.unsnoozed"
            }
            "thread.auto-settle.set" => {
                let enabled = command["enabled"]
                    .as_bool()
                    .ok_or_else(|| reject("enabled must be boolean.".into()))?;
                if enabled == thread["autoSettleDisabledAt"].is_null() {
                    thread["updatedAt"] = old_updated_at;
                }
                thread["autoSettleDisabledAt"] = if enabled {
                    Value::Null
                } else if thread["autoSettleDisabledAt"].is_null() {
                    json!(at)
                } else {
                    thread["autoSettleDisabledAt"].clone()
                };
                "thread.auto-settle-set"
            }
            "thread.pin" => {
                let pinned = !thread["pinnedAt"].is_null();
                let promotes =
                    thread["settledOverride"] == "settled" || !thread["snoozedUntil"].is_null();
                if !pinned {
                    thread["pinnedAt"] = json!(at);
                    if let Some(key) = command.get("orderKey") {
                        thread["pinOrderKey"] = key.clone();
                    }
                }
                if thread["settledOverride"] == "settled" {
                    thread["settledOverride"] = json!("active");
                    thread["settledAt"] = Value::Null;
                }
                thread["snoozedUntil"] = Value::Null;
                thread["snoozedAt"] = Value::Null;
                if pinned && !promotes {
                    thread["updatedAt"] = old_updated_at;
                }
                "thread.pinned"
            }
            "thread.unpin" => {
                if thread["pinnedAt"].is_null() {
                    thread["updatedAt"] = old_updated_at;
                }
                thread["pinnedAt"] = Value::Null;
                thread["pinOrderKey"] = Value::Null;
                "thread.unpinned"
            }
            "thread.pin.reorder" => {
                if thread["pinnedAt"].is_null() {
                    return Err(reject(format!(
                        "Thread {id} is not pinned and cannot be reordered."
                    )));
                }
                let key = required("orderKey")?;
                if thread["pinOrderKey"] == key {
                    thread["updatedAt"] = old_updated_at;
                }
                thread["pinOrderKey"] = json!(key);
                "thread.pin-reordered"
            }
            "thread.active.reorder" => {
                if !thread["pinnedAt"].is_null() || thread["settledOverride"] == "settled" {
                    return Err(reject(format!(
                        "Thread {id} is not active and cannot be reordered."
                    )));
                }
                thread["activeOrderKey"] = json!(required("orderKey")?);
                thread["updatedAt"] = old_updated_at;
                "thread.active-reordered"
            }
            "thread.metadata.update" => {
                if let Some(expected) = command.get("expectedWorktreePath") {
                    if *expected != thread["worktreePath"] {
                        return Err(reject(format!(
                            "Thread {id} worktree changed before the metadata update could be applied."
                        )));
                    }
                }
                if command["expectedEmpty"] == true
                    && (!array(projection, "messages").is_empty()
                        || !array(projection, "runs").is_empty())
                {
                    return Err(reject(format!("Thread {id} is no longer empty.")));
                }
                for field in ["limitRecovery", "linkedPullRequest"] {
                    if command.get(field).is_some() {
                        return Err(unsupported(field));
                    }
                }
                if command["regenerateTitle"] == true {
                    return Err(unsupported("title regeneration"));
                }
                for field in ["title", "branch", "worktreePath"] {
                    if let Some(value) = command.get(field) {
                        thread[field] = value.clone();
                    }
                }
                if command["regenerateTitle"] == false || command.get("title").is_some() {
                    thread["titleRegeneration"] = Value::Null;
                }
                "thread.metadata-updated"
            }
            "thread.interaction-mode.set" => {
                serde_json::from_value::<t3_contracts::ProviderInteractionMode>(
                    command["interactionMode"].clone(),
                )
                .map_err(|error| reject(error.to_string()))?;
                thread["interactionMode"] = command["interactionMode"].clone();
                "thread.interaction-mode-updated"
            }
            "thread.runtime-mode.set" => {
                if array(projection, "providerSessions").iter().any(|session| {
                    !matches!(session["status"].as_str(), Some("stopped" | "error"))
                        && session["capabilities"]["sessions"]["supportsRuntimeModeSwitchInSession"]
                            != true
                }) {
                    return Err(unsupported("provider detach for runtime mode switching"));
                }
                serde_json::from_value::<t3_contracts::RuntimeMode>(command["runtimeMode"].clone())
                    .map_err(|error| reject(error.to_string()))?;
                thread["runtimeMode"] = command["runtimeMode"].clone();
                "thread.runtime-mode-updated"
            }
            "thread.model-selection.set" => {
                let selection: t3_contracts::ModelSelection =
                    serde_json::from_value(command["modelSelection"].clone())
                        .map_err(|error| reject(error.to_string()))?;
                let selection =
                    serde_json::to_value(selection).map_err(|error| reject(error.to_string()))?;
                if selection["instanceId"] != thread["providerInstanceId"] {
                    return Err(unsupported("provider switching via handoff"));
                }
                if array(projection, "providerSessions").iter().any(|session| {
                    !matches!(session["status"].as_str(), Some("stopped" | "error"))
                        && session["capabilities"]["sessions"]["supportsModelSwitchInSession"]
                            != true
                }) {
                    return Err(unsupported("provider detach for model switching"));
                }
                thread["modelSelection"] = selection;
                "thread.model-selection-updated"
            }
            _ => return Err(unsupported(kind)),
        };
        (event_type, thread)
    };
    // Decode before committing: malformed known fields never become durable state.
    let decoded = serde_json::from_value::<t3_contracts::AppThread>(thread.clone())
        .map_err(|error| reject(error.to_string()))?;
    thread = serde_json::to_value(decoded).map_err(|error| reject(error.to_string()))?;
    let mut events = vec![Event {
        event_id: uuid::Uuid::new_v4().to_string(),
        aggregate_kind: "thread".into(),
        aggregate_id: id.into(),
        occurred_at: at,
        command_id: Some(command_id.into()),
        causation_event_id: None,
        correlation_id: Some(command_id.into()),
        event_type: event_type.into(),
        payload: thread.take(),
        metadata: json!({}),
    }];
    // Match the source lifecycle: workspace changes detach bindings in the
    // command transaction; the persisted effect unloads/reaps the runtime.
    if kind == "thread.metadata.update"
        && command
            .get("worktreePath")
            .is_some_and(|path| *path != projection.unwrap()["thread"]["worktreePath"])
    {
        for session in array(projection.unwrap(), "providerSessions")
            .iter()
            .filter(|session| !matches!(session["status"].as_str(), Some("stopped" | "error")))
        {
            events.push(Event {
                event_id: uuid::Uuid::new_v4().to_string(),
                aggregate_kind: "thread".into(), aggregate_id: id.into(), occurred_at: iso(now),
                command_id: Some(command_id.into()), causation_event_id: None,
                correlation_id: Some(command_id.into()), event_type: "provider-session.detached".into(),
                payload: json!({"providerSessionId":session["id"],"detachedAt":iso(now),"reason":"Workspace changed."}),
                metadata: json!({}),
            });
        }
    }
    Ok(events)
}

pub fn reduce(transaction: &Transaction<'_>, stored: &StoredEvent) -> Result<(), StoreError> {
    let event = &stored.event;
    let projection = projection_after(
        read_projection(transaction, "thread", &event.aggregate_id)?,
        event,
    )?;
    if event.event_type == "run.updated"
        && matches!(
            event.payload["status"].as_str(),
            Some("interrupted" | "cancelled" | "failed")
        )
    {
        transaction.execute("UPDATE rust_effect_outbox SET status='cancelled',lease_owner=NULL,lease_expires_at=NULL,updated_at=?1 WHERE thread_id=?2 AND status IN ('pending','running') AND json_extract(request_json,'$.type')='provider-turn.start' AND json_extract(request_json,'$.runId')=?3",rusqlite::params![event.occurred_at,event.aggregate_id,event.payload["id"].as_str()])?;
    }
    write_projection(transaction, "thread", &event.aggregate_id, &projection)
}
pub(crate) fn projection_after(
    projection: Option<Value>,
    event: &Event,
) -> Result<Value, StoreError> {
    let mut projection = if event.event_type == "thread.created" {
        let mut projection = json!({"thread":event.payload,"updatedAt":event.occurred_at});
        for field in [
            "runs",
            "attempts",
            "nodes",
            "subagents",
            "providerSessions",
            "providerThreads",
            "providerTurns",
            "runtimeRequests",
            "messages",
            "plans",
            "turnItems",
            "checkpointScopes",
            "checkpoints",
            "contextHandoffs",
            "contextTransfers",
            "visibleTurnItems",
        ] {
            projection[field] = json!([]);
        }
        projection
    } else {
        projection.ok_or_else(|| StoreError::InvalidProjection {
            kind: "thread".into(),
            id: event.aggregate_id.clone(),
            detail: "missing thread".into(),
        })?
    };
    let field = match event.event_type.as_str() {
        "run.created" | "run.updated" => Some("runs"),
        "run-attempt.created" | "run-attempt.updated" => Some("attempts"),
        "node.updated" => Some("nodes"),
        "provider-session.attached" | "provider-session.updated" => Some("providerSessions"),
        "provider-thread.updated" => Some("providerThreads"),
        "provider-turn.updated" => Some("providerTurns"),
        "runtime-request.updated" => Some("runtimeRequests"),
        "message.updated" => Some("messages"),
        "plan.updated" => Some("plans"),
        "turn-item.updated" => Some("turnItems"),
        _ => None,
    };
    if event.event_type == "provider-session.detached" {
        projection["providerSessions"]
            .as_array_mut()
            .ok_or_else(|| {
                StoreError::InvalidCommand("Provider sessions are not an array.".into())
            })?
            .retain(|session| session["id"] != event.payload["providerSessionId"]);
    } else if let Some(field) = field {
        let rows =
            projection[field]
                .as_array_mut()
                .ok_or_else(|| StoreError::InvalidProjection {
                    kind: "thread".into(),
                    id: event.aggregate_id.clone(),
                    detail: format!("{field} is not an array"),
                })?;
        if let Some(existing) = rows.iter_mut().find(|row| row["id"] == event.payload["id"]) {
            *existing = event.payload.clone();
        } else {
            rows.push(event.payload.clone());
        }
        if field == "providerThreads"
            && event.payload["appThreadId"] == event.aggregate_id
            && !event.payload["nativeThreadRef"].is_null()
        {
            projection["thread"]["activeProviderThreadId"] = event.payload["id"].clone();
        }
        if field == "turnItems" {
            // Local histories keep first-write order; fork visibility and rollback
            // filtering are supplied by their own lifecycle reducers when ported.
            projection["visibleTurnItems"] = Value::Array(projection["turnItems"].as_array().unwrap().iter().enumerate().map(|(position,item)|json!({"position":position,"visibility":"local","sourceThreadId":item["threadId"],"sourceItemId":item["id"],"item":item})).collect());
        }
    } else if event.event_type.starts_with("thread.") {
        projection["thread"] = event.payload.clone();
    } else {
        return Err(StoreError::InvalidCommand(format!(
            "Unsupported projection event {}",
            event.event_type
        )));
    }
    if !matches!(
        event.event_type.as_str(),
        "thread.visited" | "thread.marked-unread"
    ) {
        projection["updatedAt"] = json!(event.occurred_at);
    }
    Ok(projection)
}

pub fn shell(projection: &Value) -> Value {
    let mut shell = projection["thread"].clone();
    let latest = array(projection, "runs").last();
    let active = array(projection, "runs").iter().rev().find(|run| {
        matches!(
            string(run, "status"),
            Some("preparing" | "starting" | "running" | "waiting")
        )
    });
    shell["latestRunId"] = latest.map(|run| run["id"].clone()).unwrap_or(Value::Null);
    shell["activeRunId"] = active.map(|run| run["id"].clone()).unwrap_or(Value::Null);
    shell["status"] = latest
        .map(|run| run["status"].clone())
        .unwrap_or(json!("idle"));
    shell["pendingRuntimeRequest"] = array(projection,"runtimeRequests").iter().rev().find(|request| request["status"] == "pending").map(|request| json!({"id":request["id"],"kind":request["kind"],"createdAt":request["createdAt"]})).unwrap_or(Value::Null);
    let messages = array(projection, "messages");
    shell["latestVisibleMessage"] = messages.last().map(|message| json!({"id":message["id"],"role":message["role"],"text":message["text"],"updatedAt":message["updatedAt"]})).unwrap_or(Value::Null);
    shell["latestUserMessageAt"] = messages
        .iter()
        .rev()
        .find(|message| message["role"] == "user")
        .map(|message| message["updatedAt"].clone())
        .unwrap_or(Value::Null);
    shell["hasActionableProposedPlan"] = json!(false);
    shell["pendingBackgroundTasks"] = json!([]);
    shell["providerInstanceHistory"] = json!([]);
    shell["itemCount"] = json!(array(projection, "turnItems").len());
    shell["visibleItemCount"] = json!(array(projection, "visibleTurnItems").len());
    shell
}

pub fn wire_event(event: &StoredEvent) -> Value {
    json!({"id":event.event.event_id,"type":event.event.event_type,"threadId":event.event.aggregate_id,"providerInstanceId":event.event.payload["providerInstanceId"],"occurredAt":event.event.occurred_at,"payload":event.event.payload})
}

#[cfg(test)]
mod tests {
    use super::*;
    fn now() -> DateTime<Utc> {
        "2026-01-01T00:00:00Z".parse().unwrap()
    }
    fn create() -> Value {
        json!({"type":"thread.create","commandId":"create","threadId":"thread:1","projectId":"project:1","title":"Thread","modelSelection":{"instanceId":"codex","model":"gpt-5.6-sol"},"runtimeMode":"approval-required","interactionMode":"default","branch":null,"worktreePath":null})
    }
    fn projection() -> Value {
        let events = plan(&create(), None, Some(&json!({"deletedAt":null})), now()).unwrap();
        json!({"thread":events[0].payload,"updatedAt":iso(now()),"runs":[],"messages":[],"runtimeRequests":[],"providerSessions":[],"subagents":[]})
    }
    fn command(kind: &str, fields: Value) -> Value {
        let mut command = json!({"type":kind,"commandId":"next","threadId":"thread:1"});
        for (key, value) in fields.as_object().unwrap() {
            command[key] = value.clone();
        }
        command
    }

    #[test]
    fn model_and_runtime_changes_persist_with_live_codex_session_for_the_next_turn() {
        let mut current = projection();
        current["providerSessions"] = json!([{"status":"ready","capabilities":{"sessions":{"supportsModelSwitchInSession":true,"supportsRuntimeModeSwitchInSession":true}}}]);
        let runtime = plan(
            &command(
                "thread.runtime-mode.set",
                json!({"runtimeMode":"full-access"}),
            ),
            Some(&current),
            None,
            now(),
        )
        .unwrap();
        assert_eq!(runtime[0].event_type, "thread.runtime-mode-updated");
        current["thread"] = runtime[0].payload.clone();
        let model=plan(&command("thread.model-selection.set",json!({"modelSelection":{"instanceId":"codex","model":"fixture-model","options":[{"id":"reasoningEffort","value":"low"},{"id":"serviceTier","value":"fast"}]}})),Some(&current),None,now()).unwrap();
        assert_eq!(model[0].event_type, "thread.model-selection-updated");
        assert_eq!(model[0].payload["runtimeMode"], "full-access");
        assert_eq!(
            model[0].payload["modelSelection"]["options"][0]["value"],
            "low"
        );
        let switch = command(
            "thread.model-selection.set",
            json!({"modelSelection":{"instanceId":"claude-code","model":"opus"}}),
        );
        assert!(plan(&switch, Some(&current), None, now()).is_err());
        current["providerSessions"][0]["capabilities"]["sessions"]["supportsRuntimeModeSwitchInSession"] =
            json!(false);
        assert!(
            plan(
                &command("thread.runtime-mode.set", json!({"runtimeMode":"auto"})),
                Some(&current),
                None,
                now()
            )
            .is_err()
        );
    }

    #[test]
    fn create_projection_decodes_and_visit_is_monotonic_read_state() {
        let current = projection();
        let visit = command("thread.visit", json!({"visitedAt":"2026-01-02T00:00:00Z"}));
        let event = plan(
            &visit,
            Some(&current),
            None,
            now() + chrono::Duration::days(3),
        )
        .unwrap();
        assert_eq!(
            event[0].payload["updatedAt"],
            current["thread"]["updatedAt"]
        );
        let mut visited = current.clone();
        visited["thread"] = event[0].payload.clone();
        let older = command("thread.visit", json!({"visitedAt":"2026-01-01T00:00:00Z"}));
        assert_eq!(
            plan(&older, Some(&visited), None, now()).unwrap()[0].payload["lastVisitedAt"],
            "2026-01-02T00:00:00.000Z"
        );
    }

    #[test]
    fn pin_promotes_settled_and_snoozed_threads_and_settle_clears_pin() {
        let mut current = projection();
        current["thread"]["settledOverride"] = json!("settled");
        current["thread"]["settledAt"] = json!(iso(now()));
        current["thread"]["snoozedUntil"] = json!("2026-02-01T00:00:00Z");
        let pinned = plan(
            &command("thread.pin", json!({"orderKey":"a0"})),
            Some(&current),
            None,
            now(),
        )
        .unwrap()[0]
            .payload
            .clone();
        assert_eq!(pinned["settledOverride"], "active");
        assert!(pinned["settledAt"].is_null());
        assert!(pinned["snoozedUntil"].is_null());
        current["thread"] = pinned;
        let settled = plan(
            &command("thread.settle", json!({})),
            Some(&current),
            None,
            now(),
        )
        .unwrap()[0]
            .payload
            .clone();
        assert!(settled["pinnedAt"].is_null());
        assert!(settled["pinOrderKey"].is_null());
    }

    #[test]
    fn metadata_compare_and_set_and_expected_empty_reject_races() {
        let mut current = projection();
        current["thread"]["worktreePath"] = json!("/new");
        assert!(
            plan(
                &command(
                    "thread.metadata.update",
                    json!({"expectedWorktreePath":"/old","title":"Changed"})
                ),
                Some(&current),
                None,
                now()
            )
            .is_err()
        );
        current["messages"] = json!([{"id":"message"}]);
        assert!(
            plan(
                &command(
                    "thread.metadata.update",
                    json!({"expectedEmpty":true,"title":"Changed"})
                ),
                Some(&current),
                None,
                now()
            )
            .is_err()
        );
    }

    #[test]
    fn active_work_blocks_settle_and_pending_requests_block_snooze() {
        let mut current = projection();
        current["runs"] = json!([{"status":"running"}]);
        assert!(
            plan(
                &command("thread.settle", json!({})),
                Some(&current),
                None,
                now()
            )
            .is_err()
        );
        current["runtimeRequests"] = json!([{"status":"pending"}]);
        assert!(
            plan(
                &command(
                    "thread.snooze",
                    json!({"snoozedUntil":"2026-02-01T00:00:00Z"})
                ),
                Some(&current),
                None,
                now()
            )
            .is_err()
        );
    }

    #[test]
    fn reordering_requires_current_membership_and_preserves_activity() {
        let current = projection();
        assert!(
            plan(
                &command("thread.pin.reorder", json!({"orderKey":"a1"})),
                Some(&current),
                None,
                now()
            )
            .is_err()
        );
        let event = plan(
            &command("thread.active.reorder", json!({"orderKey":"a1"})),
            Some(&current),
            None,
            now() + chrono::Duration::days(1),
        )
        .unwrap();
        assert_eq!(
            event[0].payload["updatedAt"],
            current["thread"]["updatedAt"]
        );
    }

    #[test]
    fn persisted_commands_use_decoded_ids_titles_and_legacy_model_options() {
        let store = Store::memory().unwrap();
        let project=crate::project::ProjectCommand::from_json(json!({"type":"project.create","commandId":"project-command","projectId":"project:1","title":"Project","workspaceRoot":"/tmp/project"})).unwrap();
        crate::project::ProjectService::new(store.clone())
            .dispatch(&project, now())
            .unwrap();
        let service = ThreadService::new(store.clone());
        let mut create_command = create();
        create_command["commandId"] = json!(" create ");
        create_command["threadId"] = json!(" thread:1 ");
        create_command["projectId"] = json!(" project:1 ");
        create_command["title"] = json!(" Thread ");
        create_command["modelSelection"] = json!({"provider":"codex","model":" gpt-5.6-sol ","options":{"effort":" high ","fastMode":true}});
        assert_eq!(
            service.dispatch(&create_command, now()).unwrap().status,
            "accepted"
        );
        let projection = service.projection("thread:1").unwrap().unwrap();
        assert_eq!(projection["thread"]["title"], "Thread");
        assert_eq!(
            projection["thread"]["modelSelection"],
            json!({"instanceId":"codex","model":"gpt-5.6-sol","options":[{"id":"effort","value":"high"},{"id":"fastMode","value":true}]})
        );
        assert!(store.receipt("create").unwrap().is_some());
        assert!(store.receipt(" create ").unwrap().is_none());
        let update = command(
            "thread.metadata.update",
            json!({"title":null,"branch":null}),
        );
        let normalized = normalize_command(&update).unwrap();
        assert!(normalized.get("title").is_none());
        assert!(normalized.get("branch").unwrap().is_null());
    }
}
