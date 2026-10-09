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
        let kind = string(command, "type").unwrap_or("");
        // A fork or merge-back writes to one thread (the new one, or the one merged into) and reads another.
        let forking = matches!(kind, "thread.fork" | "thread.merge_back");
        let thread_id = string(command, if forking { "targetThreadId" } else { "threadId" }).unwrap_or("");
        self.store.dispatch(
            command_id,
            "thread",
            thread_id,
            kind,
            now,
            |transaction| {
                let current = read_projection(transaction, "thread", thread_id)?;
                if forking {
                    // The source is what the command copies from, so it is what a limited sender is checked against.
                    let source = read_projection(transaction, "thread", string(command, "sourceThreadId").unwrap_or(""))?;
                    authorize(transaction, source.as_ref())?;
                    let planned = if kind == "thread.fork" {
                        crate::thread_fork::plan_fork(command, current.as_ref(), source.as_ref(), now)
                    } else {
                        crate::thread_fork::plan_merge_back(command, current.as_ref(), source.as_ref(), now)
                    };
                    return Ok(match planned {
                        Ok(events) => Decision::Accepted { events, effects: vec![] },
                        Err(error) => Decision::Rejected(error),
                    });
                }
                authorize(transaction, current.as_ref())?;
                let project = string(command, "projectId")
                    .map(|id| read_projection(transaction, "project", id))
                    .transpose()?
                    .flatten();
                // Deleting cancels the thread's work and queues the cleanup that follows it.
                if kind == "thread.delete" {
                    let Some(current) = current.as_ref() else {
                        return Ok(Decision::Rejected(json!({"_tag":"OrchestratorDispatchError","commandId":command_id,"commandType":kind,"cause":format!("Thread {thread_id} does not exist.")})));
                    };
                    let attachments = crate::thread_deletion::attachment_ids(current);
                    return Ok(match crate::thread_deletion::plan(command, current, &attachments, now) {
                        Ok(plan) => Decision::Accepted { events: plan.events, effects: plan.effects },
                        Err(StoreError::InvalidCommand(cause)) => Decision::Rejected(json!({"_tag":"OrchestratorDispatchError","commandId":command_id,"commandType":kind,"cause":cause})),
                        Err(other) => return Err(other),
                    });
                }
                Ok(
                    match plan(command, current.as_ref(), project.as_ref(), now) {
                        Ok(events) => {
                            let effects = lifecycle_effects(command, &events, now);
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

/// The effects a thread command's events leave behind (`dispatchThreadMutation`): each detached
/// provider session is unloaded after the commit, an archive also closes the thread's terminals,
/// and a requested rename is written by an effect.
pub(crate) fn lifecycle_effects(command: &Value, events: &[Event], now: DateTime<Utc>) -> Vec<NewEffect> {
    let command_id = string(command, "commandId").unwrap_or("");
    let thread_id = string(command, "threadId").unwrap_or("");
    let kind = string(command, "type").unwrap_or("");
    let mut effects: Vec<NewEffect> = events
        .iter()
        .filter(|event| event.event_type == "provider-session.detached")
        .map(|event| {
            let mut request = json!({"type":"provider-session.detach","providerSessionId":event.payload["providerSessionId"],"detail":event.payload["reason"]});
            // Only an archive is terminal: the other detaches keep the MCP credential so a
            // re-attaching provider stays authorized.
            if kind == "thread.archive" {
                request["revokeMcpCredential"] = json!(true);
            }
            NewEffect {
                id: format!("effect:{command_id}:provider-session.detach:{}", event.payload["providerSessionId"].as_str().unwrap()),
                command_id: command_id.into(),
                thread_id: thread_id.into(),
                request,
                available_at: iso(now),
            }
        })
        .collect();
    if kind == "thread.archive" {
        effects.push(NewEffect { id: format!("effect:{command_id}:terminal.cleanup"), command_id: command_id.into(), thread_id: thread_id.into(), request: json!({"type":"terminal.cleanup"}), available_at: iso(now) });
    }
    if kind == "thread.metadata.update" && command["regenerateTitle"] == true {
        effects.push(crate::text_generation_title::generate_effect(command_id, thread_id, json!({"type":"regenerate"}), iso(now)));
    }
    effects
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

/// `thread.pull-request.link` and `.unlink`: files a pull request under the thread by its
/// host-level identity, or takes it out again. `false` when the thread already knew. A
/// stack-derived link is dismissed rather than removed, so a sync does not bring it back. The
/// branch's own pull request (`branchPullRequest`) is not tracked here yet.
fn apply_pull_request_link(thread: &mut Value, command: &Value, at: &str) -> bool {
    use crate::thread_pull_requests::{legacy_key, links, normalize};
    let key = normalize(command);
    let current = links(thread);
    let existing = current.iter().find(|link| normalize(link) == key).cloned();
    let pull_requests: Vec<Value> = if command["type"] == "thread.pull-request.link" {
        let source = command["source"].as_str().unwrap_or("manual");
        let undismisses = existing.as_ref().is_some_and(|link| link["source"] == "stack-dismissed") && !matches!(source, "stack" | "stack-dismissed");
        if existing.is_some() && !undismisses {
            return false;
        }
        let link = match existing {
            Some(mut link) => {
                link["source"] = json!(source);
                link["url"] = command["url"].clone();
                link
            }
            None => json!({"host":key["host"],"repository":key["repository"],"number":key["number"],"url":command["url"],"source":source,"linkedAt":at,"snapshot":null,"stack":null}),
        };
        current.iter().filter(|entry| normalize(entry) != key).cloned().chain([link]).collect()
    } else {
        let Some(existing) = existing else { return false };
        let in_a_stack = existing["source"] == "stack"
            || !existing["stack"].is_null()
            || current.iter().any(|link| {
                link["host"].as_str().map(str::to_lowercase) == key["host"].as_str().map(str::to_lowercase)
                    && link["repository"].as_str().map(str::to_lowercase) == key["repository"].as_str().map(str::to_lowercase)
                    && link["stack"]["layers"].as_array().is_some_and(|layers| layers.iter().any(|layer| layer["number"] == key["number"]))
            });
        current
            .iter()
            .filter_map(|link| {
                if *link != existing {
                    return Some(link.clone());
                }
                in_a_stack.then(|| {
                    let mut dismissed = link.clone();
                    dismissed.as_object_mut().map(|object| object.remove("watch"));
                    dismissed["source"] = json!("stack-dismissed");
                    dismissed
                })
            })
            .collect()
    };
    let keeps_legacy = thread
        .get("linkedPullRequest")
        .filter(|linked| !linked.is_null())
        .is_some_and(|linked| pull_requests.iter().any(|link| link["source"] != "stack-dismissed" && normalize(link) == legacy_key(linked, None)));
    if !keeps_legacy {
        thread["linkedPullRequest"] = Value::Null;
    }
    thread["pullRequests"] = json!(pull_requests);
    true
}

/// `thread.pull-request.watch`: starts or stops the server watching a linked pull request for the
/// thread's agent. A watch started on a pull request the thread has not linked (or dismissed) links
/// it in the same step. `Ok(None)` when nothing would change; a refusal is the decider's cause.
fn apply_pull_request_watch(thread: &mut Value, projection: &Value, command: &Value, at: &str) -> Result<Option<()>, String> {
    use crate::thread_pull_requests::{links, normalize};
    let id = thread["id"].as_str().unwrap_or_default().to_owned();
    let watching = command["watching"] == true;
    if watching {
        if crate::subagents::is_provider_native_subagent_thread(thread) {
            return Err(crate::subagents::READ_ONLY_REASON.into());
        }
        if thread["settledOverride"] == "settled" || !thread["settledAt"].is_null() || !thread["archivedAt"].is_null() {
            return Err(format!("Thread {id} is settled or archived and cannot watch pull requests."));
        }
        // A subagent reports to its parent thread, which owns the pull request.
        if thread["lineage"]["relationshipToParent"] == "subagent" {
            return Err(format!("Thread {id} is a subagent; its parent thread watches pull requests."));
        }
        // Only the agent's watch_pull_request links as "agent". A call that raced a Stop may land after its run
        // ended, so the latest run that executed decides.
        if command["link"]["source"] == "agent" && crate::thread_pull_requests::stop_reached_latest_run(projection) {
            return Err(format!("Thread {id} was stopped."));
        }
    }
    let key = normalize(command);
    let current = links(thread);
    let visible = |link: &Value| link["source"] != "stack-dismissed" && normalize(link) == key;
    let links_now = watching && command.get("link").is_some_and(|link| !link.is_null()) && !current.iter().any(visible);
    let next: Vec<Value> = if links_now {
        let link = json!({"host":key["host"],"repository":key["repository"],"number":key["number"],"url":command["link"]["url"],"source":command["link"]["source"],"linkedAt":at,"snapshot":null,"stack":null});
        current.iter().filter(|entry| normalize(entry) != key).cloned().chain([link]).collect()
    } else {
        current
    };
    let Some(position) = next.iter().position(visible) else { return Ok(None) };
    let previous = next[position].get("watch").filter(|watch| !watch.is_null()).cloned();
    let watch = watching.then(|| previous.clone().unwrap_or_else(|| json!({"startedAt":at,"headSha":null,"failedChecks":[],"passed":false,"passedChecks":[],"remarksThrough":at,"remarkIds":[],"conflicting":false,"wakes":0})));
    if watch == previous && !links_now {
        return Ok(None);
    }
    let mut entry = next[position].clone();
    match watch {
        Some(watch) => entry["watch"] = watch,
        None => {
            entry.as_object_mut().map(|object| object.remove("watch"));
        }
    }
    let mut pull_requests = next;
    pull_requests[position] = entry;
    thread["pullRequests"] = json!(pull_requests);
    thread["updatedAt"] = json!(at);
    Ok(Some(()))
}

/// `thread.pull-request-watch.sync` (a server command, never a client's): records what a pass over the
/// host saw, only while the watch it read is still the one on. Progress read before a stop or a restart
/// must not bring the old watch back. `false` when the thread already held this.
pub(crate) fn apply_pull_request_watch_sync(thread: &mut Value, key: &Value, started_at: &str, watch: Option<&Value>) -> bool {
    use crate::thread_pull_requests::{links, normalize};
    let current = links(thread);
    let Some(position) = current.iter().position(|link| link["source"] != "stack-dismissed" && normalize(link) == *key) else { return false };
    let existing = &current[position];
    if existing["watch"]["startedAt"] != started_at {
        return false;
    }
    let watch = watch.filter(|watch| !watch.is_null());
    if existing.get("watch").filter(|held| !held.is_null()) == watch {
        return false;
    }
    let mut entry = existing.clone();
    match watch {
        Some(watch) => entry["watch"] = watch.clone(),
        None => {
            entry.as_object_mut().map(|object| object.remove("watch"));
        }
    }
    let mut pull_requests = current;
    pull_requests[position] = entry;
    thread["pullRequests"] = json!(pull_requests);
    true
}

pub fn normalize_command(input: &Value) -> Result<Value, String> {
    let mut input = input.clone();
    if !input.is_object() {
        return Err("command must be an object".into());
    }
    if matches!(input["type"].as_str(), Some("thread.create" | "thread.fork" | "thread.merge_back")) {
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
    // An imported native thread is checked before anything is committed.
    let imported = command
        .get("importedNativeThread")
        .filter(|_| kind == "thread.create")
        .map(|imported| {
            serde_json::from_value::<t3_contracts::ImportedNativeThreadRef>(imported["ref"].clone()).map_err(|error| reject(error.to_string()))?;
            if let Some(metadata) = imported.get("metadata").filter(|metadata| !metadata.is_null()) {
                serde_json::from_value::<t3_contracts::ProviderThreadNativeMetadata>(metadata.clone()).map_err(|error| reject(error.to_string()))?;
            }
            Ok::<_, Value>(imported.clone())
        })
        .transpose()?;
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
        // Settling ends live provider sessions as archiving does, and its delegated-completion
        // cancellation is not ported, so a settle with sessions still fails explicitly instead of
        // dropping that work.
        if kind == "thread.settle"
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
                thread["archivedAt"] = json!(at);
                thread["titleRegeneration"] = Value::Null;
                // An archived thread takes no wakes, so its pull request watches end.
                for link in thread["pullRequests"].as_array_mut().into_iter().flatten() {
                    if let Some(link) = link.as_object_mut() {
                        link.remove("watch");
                    }
                }
                "thread.archived"
            }
            "thread.unarchive" => {
                if thread["archivedAt"].is_null() {
                    return Err(reject(format!("Thread {id} is not archived.")));
                }
                thread["archivedAt"] = Value::Null;
                "thread.unarchived"
            }
            // Planned with its cleanup effects by `thread_deletion`, never by this function.
            "thread.delete" => return Err(reject("thread.delete is planned by thread deletion.".into())),
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
                if command.get("limitRecovery").is_some() {
                    return Err(unsupported("limitRecovery"));
                }
                // Link (object) or unlink (null) a pull request; absent leaves it unchanged.
                if let Some(linked) = command.get("linkedPullRequest") {
                    crate::thread_pull_requests::update_legacy(&mut thread, linked, &at);
                }
                for field in ["title", "branch", "worktreePath"] {
                    if let Some(value) = command.get(field) {
                        thread[field] = value.clone();
                    }
                }
                // regenerateTitle: true arms the in-flight marker that the title effect completes;
                // a landing title or an explicit false (generation abandoned) clears it.
                if command["regenerateTitle"] == true {
                    thread["titleRegeneration"] = json!({"requestId":command_id,"startedAt":at});
                } else if command["regenerateTitle"] == false || command.get("title").is_some() {
                    thread["titleRegeneration"] = Value::Null;
                }
                "thread.metadata-updated"
            }
            "thread.pull-request.link" | "thread.pull-request.unlink" => {
                if !apply_pull_request_link(&mut thread, command, &at) {
                    // The thread already knew: nothing changes, and nothing is announced.
                    return Ok(vec![]);
                }
                "thread.pull-request-synced"
            }
            "thread.pull-request.watch" => {
                if apply_pull_request_watch(&mut thread, projection, command, &at).map_err(reject)?.is_none() {
                    // The thread already held this state: nothing changes, and nothing is announced.
                    return Ok(vec![]);
                }
                "thread.pull-request-synced"
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
                serde_json::from_value::<t3_contracts::RuntimeMode>(command["runtimeMode"].clone())
                    .map_err(|error| reject(error.to_string()))?;
                thread["runtimeMode"] = command["runtimeMode"].clone();
                "thread.runtime-mode-updated"
            }
            "thread.model-selection.set" | "provider.switch" => {
                let selection: t3_contracts::ModelSelection =
                    serde_json::from_value(command["modelSelection"].clone())
                        .map_err(|error| reject(error.to_string()))?;
                let selection =
                    serde_json::to_value(selection).map_err(|error| reject(error.to_string()))?;
                // Another provider takes the conversation over through a handoff on the next message.
                // The previous provider's session is released now, so a run still going would be cut off.
                let switching = selection["instanceId"] != thread["providerInstanceId"];
                if switching {
                    if array(projection, "runs").iter().any(crate::message_queue::blocking) {
                        return Err(reject("The provider cannot be switched while a run is active.".into()));
                    }
                    thread["providerInstanceId"] = selection["instanceId"].clone();
                } else if let crate::provider_selection_transition::SelectionTransition::Reject {
                    reason,
                } =
                    crate::provider_selection_transition::classify_selection(projection, &selection)
                {
                    return Err(reject(reason));
                }
                thread["modelSelection"] = selection;
                if switching || kind == "provider.switch" { "thread.provider-switched" } else { "thread.model-selection-updated" }
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
    // `dispatchThreadCreate`: an imported native thread arrives with its provider thread, not yet
    // loaded. Its native reference makes it the thread's active provider thread, so the first message
    // starts the agent on that session (loading or resuming it) instead of a new one.
    if let Some(imported) = imported {
        let native = &imported["ref"];
        let driver = native["driver"].as_str().unwrap_or("");
        let instance = decoded_instance(&events[0].payload);
        let encode = crate::provider_maintenance::encode_uri_component;
        let provider_thread_id = ["provider", driver, "provider-instance", instance.as_str(), "native-thread", native["nativeId"].as_str().unwrap_or("")]
            .iter()
            .fold("provider-thread".to_owned(), |id, part| format!("{id}:{}", encode(part)));
        events.push(Event {
            event_id: uuid::Uuid::new_v4().to_string(),
            aggregate_kind: "thread".into(),
            aggregate_id: id.into(),
            occurred_at: iso(now),
            command_id: Some(command_id.into()),
            causation_event_id: None,
            correlation_id: Some(command_id.into()),
            event_type: "provider-thread.updated".into(),
            payload: json!({"id":provider_thread_id,"driver":driver,"providerInstanceId":instance,"providerSessionId":null,"appThreadId":id,"ownerNodeId":null,
                "nativeThreadRef":native,"nativeConversationHeadRef":null,"status":"not_loaded","firstRunOrdinal":null,"lastRunOrdinal":null,"handoffIds":[],"forkedFrom":null,
                "pendingBackgroundTasks":[],"contextUsage":null,"nativeMetadata":imported.get("metadata").cloned().unwrap_or(Value::Null),"createdAt":iso(now),"updatedAt":iso(now)}),
            metadata: json!({}),
        });
    }
    // Archiving cancels the runs still waiting in the queue; a run that is already executing
    // is ended by the detach of its provider session below.
    if kind == "thread.archive" {
        let projection = projection.unwrap();
        let queued: Vec<&Value> = array(projection, "runs").iter().filter(|run| run["status"] == "queued").collect();
        let child = |event_type: &str, payload: Value| Event {
            event_id: uuid::Uuid::new_v4().to_string(),
            aggregate_kind: "thread".into(), aggregate_id: id.into(), occurred_at: iso(now),
            command_id: Some(command_id.into()), causation_event_id: None,
            correlation_id: Some(command_id.into()), event_type: event_type.into(),
            payload, metadata: json!({}),
        };
        for run in &queued {
            let mut run = (*run).clone();
            run["status"] = json!("cancelled");
            run["queuePosition"] = Value::Null;
            run["completedAt"] = json!(iso(now));
            events.push(child("run.updated", run));
        }
        let is_queued = |run_id: &Value| queued.iter().any(|run| run["id"] == *run_id);
        for attempt in array(projection, "attempts") {
            if is_queued(&attempt["runId"]) && matches!(string(attempt, "status"), Some("pending" | "running")) {
                let mut attempt = attempt.clone();
                attempt["status"] = json!("cancelled");
                attempt["completedAt"] = json!(iso(now));
                events.push(child("run-attempt.updated", attempt));
            }
        }
        for node in array(projection, "nodes") {
            if is_queued(&node["runId"]) && matches!(string(node, "status"), Some("pending" | "running" | "waiting")) {
                let mut node = node.clone();
                node["status"] = json!("cancelled");
                node["completedAt"] = json!(iso(now));
                events.push(child("node.updated", node));
            }
        }
    }
    // Match the source lifecycle: workspace changes detach bindings in the
    // command transaction; the persisted effect unloads/reaps the runtime.
    let detach_reason = if kind == "thread.archive" {
        Some("Thread archived.")
    } else if kind == "thread.metadata.update"
        && command
            .get("worktreePath")
            .is_some_and(|path| *path != projection.unwrap()["thread"]["worktreePath"])
    {
        Some("Workspace changed.")
    } else if kind == "thread.runtime-mode.set" {
        Some("Runtime mode changed.")
    } else if matches!(kind, "thread.model-selection.set" | "provider.switch") && command["modelSelection"]["instanceId"] != projection.unwrap()["thread"]["providerInstanceId"] {
        Some("Provider switched.")
    } else {
        None
    };
    if let Some(reason) = detach_reason {
        for session in array(projection.unwrap(), "providerSessions")
            .iter()
            .filter(|session| !matches!(session["status"].as_str(), Some("stopped" | "error")))
            .filter(|session| {
                kind != "thread.runtime-mode.set"
                    || crate::provider_selection_transition::needs_runtime_detach(session)
            })
            // A provider switch keeps what the target provider already holds.
            .filter(|session| !matches!(kind, "thread.model-selection.set" | "provider.switch") || session["providerInstanceId"] != command["modelSelection"]["instanceId"])
        {
            events.push(Event {
                event_id: uuid::Uuid::new_v4().to_string(),
                aggregate_kind: "thread".into(), aggregate_id: id.into(), occurred_at: iso(now),
                command_id: Some(command_id.into()), causation_event_id: None,
                correlation_id: Some(command_id.into()), event_type: "provider-session.detached".into(),
                payload: json!({"providerSessionId":session["id"],"detachedAt":iso(now),"reason":reason}),
                metadata: json!({}),
            });
        }
    }
    Ok(events)
}

fn decoded_instance(thread: &Value) -> String {
    thread["providerInstanceId"].as_str().unwrap_or("").to_owned()
}

pub fn reduce(transaction: &Transaction<'_>, stored: &StoredEvent) -> Result<(), StoreError> {
    let event = &stored.event;
    let mut projection = projection_after(
        read_projection(transaction, "thread", &event.aggregate_id)?,
        event,
    )?;
    // A fork shows its source's history through the fork point before its own items.
    if matches!(event.event_type.as_str(), "thread.created" | "turn-item.updated" | "run.updated" | "run-attempt.updated") && projection["thread"]["forkedFrom"]["type"] == "run" {
        let source = read_projection(transaction, "thread", projection["thread"]["forkedFrom"]["threadId"].as_str().unwrap_or(""))?;
        // A deleted source is still read, as the original's projection read does: the fork keeps its history.
        if let Some(source) = source {
            let local = projection["visibleTurnItems"].as_array().cloned().unwrap_or_default();
            projection["visibleTurnItems"] = Value::Array(crate::thread_fork::visible_with_inherited(&projection, &source, local));
        }
    }
    if event.event_type == "run.updated"
        && matches!(
            event.payload["status"].as_str(),
            Some("interrupted" | "cancelled" | "failed")
        )
    {
        transaction.execute("UPDATE rust_effect_outbox SET status='cancelled',lease_owner=NULL,lease_expires_at=NULL,updated_at=?1 WHERE thread_id=?2 AND status IN ('pending','running') AND json_extract(request_json,'$.type')='provider-turn.start' AND json_extract(request_json,'$.runId')=?3",rusqlite::params![event.occurred_at,event.aggregate_id,event.payload["id"].as_str()])?;
    }
    write_projection(transaction, "thread", &event.aggregate_id, &projection)?;
    // A fork or merge-back is listed by both threads it joins, as the original's projection reads them.
    if matches!(event.event_type.as_str(), "context-transfer.created" | "context-transfer.updated") && matches!(event.payload["type"].as_str(), Some("fork" | "merge_back")) {
        let other = if event.payload["sourceThreadId"] == event.aggregate_id { &event.payload["targetThreadId"] } else { &event.payload["sourceThreadId"] };
        if let Some(other) = other.as_str().filter(|other| *other != event.aggregate_id) {
            if let Some(mut listed) = read_projection(transaction, "thread", other)? {
                if let Some(transfers) = listed["contextTransfers"].as_array_mut() {
                    match transfers.iter_mut().find(|row| row["id"] == event.payload["id"]) {
                        Some(row) => *row = event.payload.clone(),
                        None => transfers.push(event.payload.clone()),
                    }
                    write_projection(transaction, "thread", other, &listed)?;
                }
            }
        }
    }
    Ok(())
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
        "checkpoint-scope.created" | "checkpoint-scope.updated" => Some("checkpointScopes"),
        "checkpoint.captured" => Some("checkpoints"),
        "turn-item.updated" => Some("turnItems"),
        "subagent.updated" => Some("subagents"),
        "context-transfer.created" | "context-transfer.updated" => Some("contextTransfers"),
        "context-handoff.updated" => Some("contextHandoffs"),
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
            let mut next = event.payload.clone();
            // A stale snapshot must not erase what another event recorded: a run's delegated-completion
            // cohort and a task's completion delivery are kept when the update carries none.
            for (row_field, kept) in [("runs", "delegatedCompletion"), ("subagents", "completionDelivery")] {
                if field == row_field && next.get(kept).is_none() && existing.get(kept).is_some() {
                    next[kept] = existing[kept].clone();
                }
            }
            *existing = next;
        } else {
            rows.push(event.payload.clone());
        }
        if field == "providerThreads"
            && event.payload["appThreadId"] == event.aggregate_id
            && !event.payload["nativeThreadRef"].is_null()
        {
            projection["thread"]["activeProviderThreadId"] = event.payload["id"].clone();
        }
        if matches!(field, "turnItems" | "runs" | "attempts") {
            // Local histories keep first-write order, less the runs a rollback undid and the queued
            // messages of cancelled runs; a fork's inherited rows are put in front by `reduce`.
            projection["visibleTurnItems"] = Value::Array(crate::thread_fork::local_visible_rows(&projection));
        }
    } else if event.event_type == "checkpoint.rollback-requested" {
        // A request only: the rollback effect's own events change the projection.
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
    // The run the thread is doing work for, waiting included (`isActivityRunForShell`).
    let activity = array(projection, "runs").iter().filter(|run| matches!(string(run, "status"), Some("preparing" | "starting" | "running" | "waiting"))).max_by_key(|run| run["ordinal"].as_u64());
    shell["activityRunStatus"] = activity.map_or(Value::Null, |run| run["status"].clone());
    shell["activityRunStartedAt"] = activity.map_or(Value::Null, |run| [&run["workStartedAt"], &run["startedAt"], &run["requestedAt"]].into_iter().find(|value| !value.is_null()).cloned().unwrap_or(Value::Null));
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
    shell["pendingBackgroundTasks"] = json!(crate::background_work::derive_for(projection));
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

    /// Archiving and unarchiving decide the same events and effects as the original, for
    /// threads with and without queued runs, live sessions and watched pull requests.
    #[test]
    fn archiving_matches_the_original() {
        let at: DateTime<Utc> = "2026-09-04T00:00:00.000Z".parse().unwrap();
        let mut compared = 0;
        for line in include_str!("../tests/fixtures/thread-archive.jsonl").lines() {
            let row: Value = serde_json::from_str(line).unwrap();
            let name = row["name"].as_str().unwrap();
            let planned = plan(&row["command"], Some(&row["projection"]), None, at);
            if let Some(refused) = row["refused"].as_str() {
                assert_eq!(planned.unwrap_err()["cause"], refused, "{name}");
                compared += 1;
                continue;
            }
            let events = planned.unwrap_or_else(|error| panic!("{name}: {error}"));
            // The stored thread spells an absent pull request list as null; the wire projection omits it.
            let wire: Vec<Value> = events.iter().map(|event| {
                let mut payload = event.payload.clone();
                if payload["pullRequests"].is_null() {
                    payload.as_object_mut().unwrap().remove("pullRequests");
                }
                json!({"type":event.event_type,"payload":payload})
            }).collect();
            let expected: Vec<Value> = row["events"].as_array().unwrap().iter().map(|event| json!({"type":event["type"],"payload":event["payload"]})).collect();
            assert_eq!(Value::Array(wire), Value::Array(expected), "{name}");
            let effects: Vec<Value> = lifecycle_effects(&row["command"], &events, at).iter().map(|effect| json!({"id":effect.id,"request":effect.request})).collect();
            assert_eq!(Value::Array(effects), row["effects"], "{name}");
            compared += 1;
        }
        assert_eq!(compared, 14);
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
        // Another provider is chosen with a handoff to follow; the old provider's session is released.
        let switched = plan(&switch, Some(&current), None, now()).unwrap();
        assert_eq!(switched[0].event_type, "thread.provider-switched");
        current["providerSessions"][0]["capabilities"]["sessions"]["supportsRuntimeModeSwitchInSession"] =
            json!(false);
        current["providerSessions"][0]["id"] = json!("session:mode");
        let detached = plan(
            &command("thread.runtime-mode.set", json!({"runtimeMode":"auto"})),
            Some(&current),
            None,
            now(),
        )
        .unwrap();
        assert_eq!(detached.len(), 2);
        assert_eq!(detached[1].event_type, "provider-session.detached");
        assert_eq!(detached[1].payload["reason"], "Runtime mode changed.");
    }

    #[test]
    fn acp_selection_uses_bound_negotiation_and_detached_native_thread_falls_back_to_adapter() {
        let mut current = projection();
        current["thread"]["providerInstanceId"] = json!("agent");
        current["thread"]["modelSelection"] = json!({"instanceId":"agent","model":"first"});
        current["thread"]["activeProviderThreadId"] = json!("provider-thread");
        current["providerThreads"] = json!([{"id":"provider-thread","providerInstanceId":"agent","driver":"acpRegistry","nativeThreadRef":{"nativeId":"native"}}]);
        current["providerSessions"] = json!([
            {"id":"old","driver":"acpRegistry","providerInstanceId":"agent","status":"ready","updatedAt":"2026-01-01T00:00:00Z","capabilities":{"sessions":{"supportsModelSwitchInSession":false}}},
            {"id":"new","driver":"acpRegistry","providerInstanceId":"agent","status":"ready","updatedAt":"2026-01-02T00:00:00Z","capabilities":{"sessions":{"supportsModelSwitchInSession":true}}},
            {"id":"unrelated","driver":"acpRegistry","providerInstanceId":"other","status":"ready","updatedAt":"2026-01-03T00:00:00Z","capabilities":{"sessions":{"supportsModelSwitchInSession":false}}}
        ]);
        let change = command(
            "thread.model-selection.set",
            json!({"modelSelection":{"instanceId":"agent","model":"second"}}),
        );
        assert_eq!(plan(&change, Some(&current), None, now()).unwrap().len(), 1);
        current["providerSessions"][1]["capabilities"]["sessions"]["supportsModelSwitchInSession"] =
            json!(false);
        let rejection = plan(&change, Some(&current), None, now()).unwrap_err();
        assert!(
            rejection
                .to_string()
                .contains("The active ACP session does not expose a model-switch capability.")
        );
        let unchanged = command(
            "thread.model-selection.set",
            json!({"modelSelection":{"instanceId":"agent","model":"first"}}),
        );
        let options_only = command(
            "thread.model-selection.set",
            json!({"modelSelection":{"instanceId":"agent","model":"first","options":[{"id":"effort","value":"high"}]}}),
        );
        assert_eq!(
            plan(&unchanged, Some(&current), None, now()).unwrap().len(),
            1
        );
        assert_eq!(
            plan(&options_only, Some(&current), None, now())
                .unwrap()
                .len(),
            1
        );
        // Detach removes bindings. Original planning uses adapter capabilities,
        // rather than recovering negotiated state from the unbound global row.
        current["providerSessions"] = json!([]);
        assert!(
            plan(&change, Some(&current), None, now())
                .unwrap_err()
                .to_string()
                .contains("model-switch capability")
        );
        assert_eq!(
            plan(&options_only, Some(&current), None, now())
                .unwrap()
                .len(),
            1
        );
        current["providerThreads"] = json!([]);
        // A fresh thread has no current native session to constrain selection.
        assert_eq!(plan(&change, Some(&current), None, now()).unwrap().len(), 1);
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
