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
                // A provider switch releases the sessions its transition names (`ProviderSwitchService`).
                let mut switched;
                let command = if matches!(kind, "thread.model-selection.set" | "provider.switch") && current.as_ref().is_some_and(|current| command["modelSelection"]["instanceId"] != current["thread"]["modelSelection"]["instanceId"]) {
                    let registry = crate::provider_switch::registry(&self.store);
                    switched = command.clone();
                    match crate::provider_switch::plan(registry.as_ref(), current.as_ref().unwrap(), &command["modelSelection"]) {
                        Ok(plan) => switched["releaseProviderSessionIds"] = json!(plan.release),
                        Err(reason) => return Ok(Decision::Rejected(json!({"_tag":"OrchestratorDispatchError","commandId":command_id,"commandType":kind,"cause":reason}))),
                    }
                    &switched
                } else {
                    command
                };
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
            let mut rows = read_projections(connection,"thread")?;
            // The original lists threads by when they last changed, then by id.
            rows.sort_by(|left, right| (left["thread"]["updatedAt"].as_str(), left["thread"]["id"].as_str()).cmp(&(right["thread"]["updatedAt"].as_str(), right["thread"]["id"].as_str())));
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
/// branch's own pull request (`branchPullRequest`, recorded by `thread.pull-request.sync`) is folded in by a manual link.
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
        // A pull request the user links by hand to a thread whose branch has one of its own, and that has no
        // link yet, keeps the branch's pull request beside it.
        let branch = thread.get("branchPullRequest").filter(|branch| !branch.is_null());
        let branch_link = branch.filter(|_| source == "manual" && crate::thread_pull_requests::visible(&current).is_empty()).and_then(|branch| {
            let branch_key = legacy_key(branch, None);
            (normalize(&branch_key) != key && !current.iter().any(|entry| normalize(entry) == normalize(&branch_key)))
                .then(|| json!({"host":branch_key["host"],"repository":branch_key["repository"],"number":branch_key["number"],"url":branch["url"],"source":"manual","linkedAt":at,"snapshot":null,"stack":null}))
        });
        branch_link.into_iter().chain(current.iter().filter(|entry| normalize(entry) != key).cloned()).chain([link]).collect()
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
    let mut normalized=normalize_command(command).map_err(|detail|json!({"_tag":"OrchestratorDispatchError","commandId":command["commandId"],"commandType":command["type"],"cause":detail}))?;
    // The sessions a provider switch releases, decided by the service that planned the switch.
    if let Some(release)=command.get("releaseProviderSessionIds"){normalized["releaseProviderSessionIds"]=release.clone();}
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
    // Automatic settlement evaluated a snapshot of the thread: anything that changed since, or any
    // explicit override (including the un-settle button's "active"), wins over the sweep.
    if kind == "thread.auto-settle" {
        let current = projection.ok_or_else(|| reject(format!("Thread {id} does not exist.")))?;
        let snapshot = time(&command["snapshotAt"]);
        if !current["thread"]["settledOverride"].is_null() || time(&current["thread"]["updatedAt"]).zip(snapshot).is_none_or(|(updated, snapshot)| updated > snapshot) {
            return Err(reject(format!("Thread {id} changed before automatic settlement.")));
        }
        let mut settle = json!({"type":"thread.settle","commandId":command_id,"threadId":id});
        if let Some(settled_at) = command.get("settledAt").filter(|value| !value.is_null()) {
            settle["settledAt"] = settled_at.clone();
        }
        return plan(&settle, projection, project, now);
    }
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
    // Queued runs a settle cancels with it: the deliveries that only wake the agent.
    let mut settled_deliveries: Vec<Value> = vec![];
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
                // Queued deliveries that only wake the agent are cancelled with the settle; a pending
                // question the agent can answer by message is none here, since this server holds no such request.
                settled_deliveries = automatic.iter().map(|run| run["id"].clone()).collect();
                if !requests.is_empty() {
                    return Err(unsupported("settle with a pending message-capable question"));
                }
                // A settled thread takes no wakes, so its pull request watches end.
                for link in thread["pullRequests"].as_array_mut().into_iter().flatten() {
                    if let Some(link) = link.as_object_mut() {
                        link.remove("watch");
                    }
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
                // Arm (object) or clear (null) the recovery from a usage limit; absent leaves it unchanged.
                if let Some(recovery) = command.get("limitRecovery") {
                    if !recovery.is_null() {
                        crate::usage_limits::validate_recovery(projection, recovery, now).map_err(reject)?;
                    }
                    crate::usage_limits::apply_recovery(&mut thread, command, recovery, now);
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
    // Settling cancels the deliveries that only wake the agent, as a user cancelling each would.
    for run_id in &settled_deliveries {
        let cancel = json!({"type":"queued-run.cancel","commandId":command_id,"threadId":id,"runId":run_id});
        if let Ok(Decision::Accepted { events: cancelled, .. }) = crate::queued_runs::plan_cancel(&cancel, projection.unwrap(), now) {
            events.extend(cancelled);
        }
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
    } else if kind == "thread.settle" {
        Some("Thread settled.")
    } else if kind == "thread.runtime-mode.set" {
        Some("Runtime mode changed.")
    } else if matches!(kind, "thread.model-selection.set" | "provider.switch") && command["modelSelection"]["instanceId"] != projection.unwrap()["thread"]["providerInstanceId"] {
        Some("Provider or model selection changed.")
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
            // A provider switch releases the sessions its transition names, and keeps what the target already holds.
            .filter(|session| !matches!(kind, "thread.model-selection.set" | "provider.switch") || match command["releaseProviderSessionIds"].as_array() {
                Some(release) => release.contains(&session["id"]),
                None => session["providerInstanceId"] != command["modelSelection"]["instanceId"],
            })
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
            crate::shell_state::inherit(&mut projection, &source);
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
    crate::shared_rows::apply(transaction, event)?;
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
/// Events whose payload is the whole thread record, and that therefore stamp their own `updatedAt`. Every other event of a thread
/// moves the thread's `updatedAt` to the time it occurred (`ProjectionStore.apply`, which lists these and not the two the list
/// forgot, `thread.active-reordered` and `thread.pull-request-synced`; they are moved too, as the original does).
const STAMP_THEIR_OWN: [&str; 19] = [
    "thread.created", "thread.archived", "thread.unarchived", "thread.deleted", "thread.settled", "thread.unsettled", "thread.snoozed", "thread.unsnoozed",
    "thread.auto-settle-set", "thread.pinned", "thread.unpinned", "thread.pin-reordered", "thread.visited", "thread.marked-unread", "thread.metadata-updated",
    "thread.runtime-mode-updated", "thread.interaction-mode-updated", "thread.model-selection-updated", "thread.provider-switched",
];

/// `isQueuedProviderThreadPlaceholder`: a provider thread reserved for a queued run, not yet the thread's.
fn queued_provider_thread_placeholder(provider_thread: &Value) -> bool {
    provider_thread["status"] == "not_loaded" && provider_thread["firstRunOrdinal"].is_null() && provider_thread["nativeThreadRef"].is_null() && provider_thread["providerSessionId"].is_null()
}

/// One event applied to a thread's projection: `applyToProjection` and the SQL projection the original reads. The importer runs
/// the events the original wrote through this too, so there is one reducer.
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
        projection[crate::shell_state::KEY] = crate::shell_state::empty();
        projection
    } else {
        projection.ok_or_else(|| StoreError::InvalidProjection {
            kind: "thread".into(),
            id: event.aggregate_id.clone(),
            detail: "missing thread".into(),
        })?
    };
    // A thread stored by an earlier build has no counters yet: they are made from its history, once, and kept from here on.
    if projection.get(crate::shell_state::KEY).is_none_or(|state| !state.is_object()) {
        projection[crate::shell_state::KEY] = crate::shell_state::scan(&projection);
    }
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
    } else if event.event_type == "run.background-work-cancelled" {
        // Only this field of the run changes, so a lifecycle write that raced it is not undone.
        for run in projection["runs"].as_array_mut().into_iter().flatten().filter(|run| run["id"] == event.payload["runId"]) {
            run["restartCancelledBackgroundWork"] = event.payload["restartCancelledBackgroundWork"].clone();
        }
    } else if let Some(field) = field {
        let rows =
            projection[field]
                .as_array_mut()
                .ok_or_else(|| StoreError::InvalidProjection {
                    kind: "thread".into(),
                    id: event.aggregate_id.clone(),
                    detail: format!("{field} is not an array"),
                })?;
        let replaced = if let Some(existing) = rows.iter_mut().find(|row| row["id"] == event.payload["id"]) {
            let before = crate::shell_state::before(field, existing);
            let mut next = event.payload.clone();
            // A stale snapshot must not erase what another event recorded: a run's delegated-completion
            // cohort and the work a restart cancelled, a task's completion delivery, and a provider turn's
            // token usage are kept when the update carries none.
            for (row_field, kept) in [("runs", "delegatedCompletion"), ("runs", "restartCancelledBackgroundWork"), ("subagents", "completionDelivery"), ("providerTurns", "tokenUsage")] {
                if field == row_field && next.get(kept).is_none() && existing.get(kept).is_some() {
                    next[kept] = existing[kept].clone();
                }
            }
            *existing = next;
            Some(before)
        } else {
            rows.push(event.payload.clone());
            None
        };
        crate::shell_state::applied(&mut projection, field, &event.payload, replaced);
        // A provider thread that is not a queued placeholder becomes the thread's own.
        if field == "providerThreads"
            && event.payload["appThreadId"] == event.aggregate_id
            && !queued_provider_thread_placeholder(&event.payload)
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
    if !STAMP_THEIR_OWN.contains(&event.event_type.as_str()) {
        projection["thread"]["updatedAt"] = json!(event.occurred_at);
    }
    // The projection is read as of its thread's last update.
    projection["updatedAt"] = projection["thread"]["updatedAt"].clone();
    Ok(projection)
}

/// The thread shell the original builds from a thread's projection (`threadShellFromProjection`, `selectShellThreadRows`), field for
/// field: what the sidebar needs and nothing of the transcript (`latestVisibleMessage` is always null, as the original sends it).
/// The counters come from `shell_state`, kept by the reducer; the rest looks at runs, requests, plans and provider threads, which
/// are few.
pub fn shell(projection: &Value) -> Value {
    let mut shell = projection["thread"].clone();
    // The rollback bookkeeping belongs to the thread record only; the shell schema does not carry it.
    if let Some(record) = shell.as_object_mut() {
        record.remove("rollbackRequestId");
        record.remove("rollbackFailure");
        // The fields the original always sends, null when the thread says nothing.
        for field in ["unsettledAt", "snoozedUntil", "snoozedAt", "pinnedAt", "autoSettleDisabledAt", "pinOrderKey", "titleRegeneration", "limitRecovery"] {
            record.entry(field).or_insert(Value::Null);
        }
    }
    shell["pullRequests"] = json!(crate::thread_pull_requests::links(&projection["thread"]));
    // The run the thread's outcome is read from, and why it stopped (`orchestrationV2ThreadError`).
    let (latest, last_error, last_error_class, usage_limit_reset_at) = crate::usage_limits::shell_fields(projection);
    shell["lastError"] = last_error;
    shell["lastErrorClass"] = last_error_class;
    shell["usageLimitResetAt"] = usage_limit_reset_at;
    let runs = array(projection, "runs");
    let newest = |statuses: &[&str]| runs.iter().filter(|run| statuses.iter().any(|status| run["status"] == *status)).max_by_key(|run| (run["ordinal"].as_u64(), run["id"].as_str()));
    // Interruptible runs; a run waiting on a person is not one (`isInterruptibleRunForShell`).
    let active = newest(&["preparing", "starting", "running"]);
    shell["latestRunId"] = latest.map(|run| run["id"].clone()).unwrap_or(Value::Null);
    for (field, source) in [("latestRunRequestedAt", "requestedAt"), ("latestRunStartedAt", "startedAt"), ("latestRunCompletedAt", "completedAt")] {
        shell[field] = latest.and_then(|run| run.get(source)).cloned().unwrap_or(Value::Null);
    }
    shell["activeRunId"] = active.map(|run| run["id"].clone()).unwrap_or(Value::Null);
    // The run the thread is doing work for, waiting included (`isActivityRunForShell`).
    let activity = newest(&["preparing", "starting", "running", "waiting"]);
    shell["activityRunStatus"] = activity.map_or(Value::Null, |run| run["status"].clone());
    shell["activityRunStartedAt"] = activity.map_or(Value::Null, |run| [&run["workStartedAt"], &run["startedAt"], &run["requestedAt"]].into_iter().find(|value| !value.is_null()).cloned().unwrap_or(Value::Null));
    shell["status"] = latest
        .map(|run| run["status"].clone())
        .unwrap_or(json!("idle"));
    // The newest pending request; or, for a secret a card asks for, that card of a run that is working (`secretRequestAsPendingInput`).
    let request = array(projection, "runtimeRequests").iter().filter(|request| request["status"] == "pending").max_by_key(|request| (request["createdAt"].as_str(), request["id"].as_str()));
    shell["pendingRuntimeRequest"] = match request {
        Some(request) => json!({"id":request["id"],"kind":request["kind"],"createdAt":request["createdAt"]}),
        None if activity.is_some() => array(projection, "turnItems")
            .iter()
            .filter(|item| item["type"] == "secret_request" && item["status"] == "waiting" && !item["nodeId"].is_null())
            .filter(|item| runs.iter().any(|run| run["id"] == item["runId"] && matches!(run["status"].as_str(), Some("preparing" | "starting" | "running" | "waiting"))))
            .max_by_key(|item| (item["updatedAt"].as_str(), item["id"].as_str()))
            .map_or(Value::Null, |item| json!({"id":item["id"],"kind":"user_input","createdAt":if item["startedAt"].is_null() { &item["updatedAt"] } else { &item["startedAt"] }})),
        None => Value::Null,
    };
    // Message bodies belong to the thread's own read; a shell stays the same size however long the conversation is.
    shell["latestVisibleMessage"] = Value::Null;
    let state = crate::shell_state::of(projection);
    shell["latestUserMessageAt"] = state["userMessage"]["at"].clone();
    shell["latestUserAuthoredMessageAt"] = state["authoredMessage"]["at"].clone();
    shell["hasActionableProposedPlan"] = json!(array(projection, "plans").iter().any(|plan| plan["kind"] == "proposed_plan" && plan["status"] == "active"));
    shell["pendingBackgroundTasks"] = json!(crate::background_work::derive_for(projection));
    shell["providerInstanceHistory"] = json!(provider_instance_history(projection));
    // The native goal lives on the provider thread that owns the conversation.
    shell["goal"] = array(projection, "providerThreads").iter().find(|thread| thread["id"] == projection["thread"]["activeProviderThreadId"]).and_then(|thread| thread.get("goal")).cloned().unwrap_or(Value::Null);
    shell["itemCount"] = json!(crate::shell_state::item_count(projection));
    shell["visibleItemCount"] = json!(crate::shell_state::visible_item_count(projection));
    shell
}

/// Provider instances that have owned the thread's root conversation, oldest first. A subagent's provider thread has an owner node
/// and is left out, so a delegated Codex child does not make a Claude thread look handed off (`providerInstanceHistoryForShell`).
fn provider_instance_history(projection: &Value) -> Vec<Value> {
    let mut threads: Vec<&Value> = array(projection, "providerThreads").iter().filter(|thread| thread["appThreadId"] == projection["thread"]["id"] && thread["ownerNodeId"].is_null()).collect();
    threads.sort_by(|left, right| (left["createdAt"].as_str(), left["id"].as_str()).cmp(&(right["createdAt"].as_str(), right["id"].as_str())));
    let mut history: Vec<Value> = Vec::new();
    for thread in threads {
        if !history.contains(&thread["providerInstanceId"]) {
            history.push(thread["providerInstanceId"].clone());
        }
    }
    history
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

    // ---- the reducer: the original's rules (`applyToProjection`, `ProjectionStore.apply`) ----

    fn event(thread: &str, event_type: &str, payload: Value, at: &str) -> Event {
        Event { event_id: format!("event:{event_type}:{at}"), aggregate_kind: "thread".into(), aggregate_id: thread.into(), occurred_at: at.into(), command_id: None, causation_event_id: None, correlation_id: None, event_type: event_type.into(), payload, metadata: json!({}) }
    }
    fn fixture_payloads() -> Vec<(String, Value)> {
        use std::io::Read;
        let mut text = String::new();
        flate2::read::GzDecoder::new(&include_bytes!("../tests/fixtures/event-payloads.jsonl.gz")[..]).read_to_string(&mut text).unwrap();
        text.lines().map(|line| serde_json::from_str::<Value>(line).unwrap()).filter(|row| row["aggregate"] == "thread").map(|row| (row["type"].as_str().unwrap().to_owned(), row["canonical"].clone())).collect()
    }
    fn created() -> Value {
        static BASE: std::sync::LazyLock<Value> = std::sync::LazyLock::new(|| {
            let (_, payload) = fixture_payloads().into_iter().find(|(kind, _)| kind == "thread.created").unwrap();
            let mut payload = t3_contracts::normalize_event_payload("thread.created", payload).unwrap();
            payload["id"] = json!("thread:rules");
            payload["activeProviderThreadId"] = Value::Null;
            projection_after(None, &event("thread:rules", "thread.created", payload, "2026-01-01T00:00:00.000Z")).unwrap()
        });
        BASE.clone()
    }

    /// Every event type the original can write is reduced, and what it says is kept: a thread event is the thread record, anything
    /// else is the row of its list (or, for a session detached or a rollback request, the lists it leaves alone).
    #[test]
    fn every_event_type_the_original_writes_is_reduced_and_kept() {
        let mut types = std::collections::BTreeSet::new();
        for (kind, canonical) in fixture_payloads() {
            let payload = t3_contracts::normalize_event_payload(&kind, canonical.clone()).unwrap();
            let mut base = created();
            let at = "2026-01-02T03:04:05.000Z";
            let after = projection_after(Some(base.clone()), &event("thread:rules", &kind, payload.clone(), at)).unwrap_or_else(|error| panic!("{kind}: {error}"));
            types.insert(kind.clone());
            if kind.starts_with("thread.") {
                // The thread event carries the whole record.
                let mut expected = payload.clone();
                if !STAMP_THEIR_OWN.contains(&kind.as_str()) {
                    expected["updatedAt"] = json!(at);
                }
                assert_eq!(after["thread"], expected, "{kind}");
                continue;
            }
            let list = match kind.as_str() {
                "run.created" | "run.updated" => "runs",
                "run-attempt.created" | "run-attempt.updated" => "attempts",
                "node.updated" => "nodes",
                "provider-session.attached" | "provider-session.updated" => "providerSessions",
                "provider-thread.updated" => "providerThreads",
                "provider-turn.updated" => "providerTurns",
                "runtime-request.updated" => "runtimeRequests",
                "message.updated" => "messages",
                "turn-item.updated" => "turnItems",
                "plan.updated" => "plans",
                "checkpoint-scope.created" => "checkpointScopes",
                "checkpoint.captured" => "checkpoints",
                "context-handoff.updated" => "contextHandoffs",
                "context-transfer.created" | "context-transfer.updated" => "contextTransfers",
                "subagent.updated" => "subagents",
                // These leave the lists as they are: nothing to detach, no run to mark, a request only.
                "provider-session.detached" | "run.background-work-cancelled" | "checkpoint.rollback-requested" => {
                    for list in ["providerSessions", "runs", "checkpoints"] {
                        assert_eq!(after[list], base[list], "{kind}");
                    }
                    continue;
                }
                other => panic!("{other} is an event type of the original this test does not place"),
            };
            assert_eq!(after[list][0], payload, "{kind}");
            base = after;
            assert_eq!(base["thread"]["updatedAt"], at, "{kind} moves the thread's updatedAt");
        }
        assert_eq!(types.len(), 43, "{types:?}");
    }

    #[test]
    fn a_provider_turn_keeps_its_token_usage_and_a_run_the_work_a_restart_cancelled_when_an_update_omits_them() {
        let base = created();
        let turn = json!({"id":"turn","providerThreadId":"pt","nodeId":"n","runAttemptId":null,"nativeTurnRef":null,"ordinal":1,"status":"running","startedAt":null,"completedAt":null,"tokenUsage":{"usedTokens":10,"updatedAt":"2026-01-01T00:00:01.000Z"}});
        let mut later = turn.clone();
        later["status"] = json!("completed");
        later.as_object_mut().unwrap().remove("tokenUsage");
        let after = [("provider-turn.updated", &turn), ("provider-turn.updated", &later)].iter().fold(base.clone(), |projection, (kind, payload)| projection_after(Some(projection), &event("thread:rules", kind, (*payload).clone(), "2026-01-01T00:00:02.000Z")).unwrap());
        assert_eq!((after["providerTurns"][0]["status"].clone(), after["providerTurns"][0]["tokenUsage"]["usedTokens"].clone()), (json!("completed"), json!(10)));
        // A report of newer usage replaces it.
        let mut newer = later.clone();
        newer["tokenUsage"] = json!({"usedTokens":99,"updatedAt":"2026-01-01T00:00:03.000Z"});
        let after = projection_after(Some(after), &event("thread:rules", "provider-turn.updated", newer, "2026-01-01T00:00:03.000Z")).unwrap();
        assert_eq!(after["providerTurns"][0]["tokenUsage"]["usedTokens"], 99);

        let run = json!({"id":"run","threadId":"thread:rules","ordinal":1,"providerInstanceId":"codex","modelSelection":{"instanceId":"codex","model":"m"},"providerThreadId":null,"userMessageId":"m","rootNodeId":"root","activeAttemptId":null,"status":"running","requestedAt":"2026-01-01T00:00:00.000Z","startedAt":null,"completedAt":null,"checkpointId":null,"contextHandoffId":null});
        let other = json!({"id":"other","threadId":"thread:rules","ordinal":2,"providerInstanceId":"codex","modelSelection":{"instanceId":"codex","model":"m"},"providerThreadId":null,"userMessageId":"m2","rootNodeId":"root2","activeAttemptId":null,"status":"queued","requestedAt":"2026-01-01T00:00:00.000Z","startedAt":null,"completedAt":null,"checkpointId":null,"contextHandoffId":null});
        let mut projection = projection_after(Some(base), &event("thread:rules", "run.created", run.clone(), "2026-01-01T00:00:01.000Z")).unwrap();
        projection = projection_after(Some(projection), &event("thread:rules", "run.created", other, "2026-01-01T00:00:01.000Z")).unwrap();
        // The work a restart cancelled is recorded on its run alone, and a later update of that run does not erase it.
        let work = json!([{"taskId":"t","description":"d","kind":"command"}]);
        projection = projection_after(Some(projection), &event("thread:rules", "run.background-work-cancelled", json!({"runId":"run","restartCancelledBackgroundWork":work}), "2026-01-01T00:00:02.000Z")).unwrap();
        assert_eq!((projection["runs"][0]["restartCancelledBackgroundWork"].clone(), projection["runs"][1].get("restartCancelledBackgroundWork").cloned()), (work.clone(), None));
        let mut ended = run;
        ended["status"] = json!("completed");
        projection = projection_after(Some(projection), &event("thread:rules", "run.updated", ended, "2026-01-01T00:00:03.000Z")).unwrap();
        assert_eq!((projection["runs"][0]["status"].clone(), projection["runs"][0]["restartCancelledBackgroundWork"].clone()), (json!("completed"), work));
    }

    #[test]
    fn a_provider_thread_becomes_the_threads_own_unless_it_is_a_queued_placeholder() {
        let thread = |id: &str, app: &str, status: &str, first: Value, native: Value, session: Value| json!({"id":id,"driver":"codex","providerInstanceId":"codex","providerSessionId":session,"appThreadId":app,"ownerNodeId":null,"nativeThreadRef":native,"nativeConversationHeadRef":null,"status":status,"firstRunOrdinal":first,"lastRunOrdinal":null,"handoffIds":[],"forkedFrom":null,"createdAt":"2026-01-01T00:00:00.000Z","updatedAt":"2026-01-01T00:00:00.000Z"});
        let apply = |projection: Value, payload: Value| projection_after(Some(projection), &event("thread:rules", "provider-thread.updated", payload, "2026-01-01T00:00:01.000Z")).unwrap();
        let native = json!({"driver":"codex","nativeId":"n","strength":"strong"});
        // A reserved thread for a queued run is not the thread's yet.
        let reserved = apply(created(), thread("reserved", "thread:rules", "not_loaded", Value::Null, Value::Null, Value::Null));
        assert!(reserved["thread"]["activeProviderThreadId"].is_null());
        // One with no native reference but a session, an ordinal or a status of its own is (this server used to ask for a reference).
        for payload in [thread("a", "thread:rules", "idle", Value::Null, Value::Null, Value::Null), thread("b", "thread:rules", "not_loaded", json!(1), Value::Null, Value::Null), thread("c", "thread:rules", "not_loaded", Value::Null, Value::Null, json!("session")), thread("d", "thread:rules", "not_loaded", Value::Null, native, Value::Null)] {
            let id = payload["id"].clone();
            assert_eq!(apply(created(), payload)["thread"]["activeProviderThreadId"], id);
        }
        // Another app thread's provider thread is not this thread's.
        assert!(apply(created(), thread("e", "thread:other", "idle", json!(1), Value::Null, Value::Null))["thread"]["activeProviderThreadId"].is_null());
    }

    #[test]
    fn the_shell_is_what_the_original_sends() {
        let mut projection = created();
        fn apply(projection: &mut Value, kind: &str, payload: Value, at: &str) {
            *projection = projection_after(Some(projection.clone()), &event("thread:rules", kind, payload, at)).unwrap();
        }
        let run = |id: &str, ordinal: u64, status: &str, started: Value| json!({"id":id,"threadId":"thread:rules","ordinal":ordinal,"providerInstanceId":"codex","modelSelection":{"instanceId":"codex","model":"m"},"providerThreadId":null,"userMessageId":"m","rootNodeId":format!("root:{id}"),"activeAttemptId":null,"status":status,"requestedAt":"2026-01-01T00:00:01.000Z","startedAt":started,"completedAt":if status == "completed" { json!("2026-01-01T00:00:09.000Z") } else { Value::Null },"checkpointId":null,"contextHandoffId":null});
        let message = |id: &str, role: &str, by: &str, at: &str| json!({"createdBy":by,"creationSource":"web","id":id,"threadId":"thread:rules","runId":null,"nodeId":null,"role":role,"text":"body that is not sent","attachments":[],"streaming":false,"createdAt":at,"updatedAt":at});
        let item = |id: &str, ordinal: u64, run: Option<&str>| json!({"id":id,"threadId":"thread:rules","runId":run,"nodeId":null,"providerThreadId":null,"providerTurnId":null,"nativeItemRef":null,"parentItemId":null,"ordinal":ordinal,"status":"completed","title":null,"startedAt":null,"completedAt":null,"updatedAt":"2026-01-01T00:00:01.000Z","type":"system_notice","message":"x"});
        apply(&mut projection, "run.created", run("r1", 1, "completed", json!("2026-01-01T00:00:02.000Z")), "2026-01-01T00:00:03.000Z");
        apply(&mut projection, "run.created", run("r2", 2, "waiting", json!("2026-01-01T00:00:04.000Z")), "2026-01-01T00:00:05.000Z");
        apply(&mut projection, "message.updated", message("a", "user", "user", "2026-01-01T00:00:06.000Z"), "2026-01-01T00:00:06.000Z");
        apply(&mut projection, "message.updated", message("b", "user", "agent", "2026-01-01T00:00:07.000Z"), "2026-01-01T00:00:07.000Z");
        apply(&mut projection, "message.updated", message("c", "assistant", "agent", "2026-01-01T00:00:08.000Z"), "2026-01-01T00:00:08.000Z");
        for (id, ordinal, run) in [("i1", 1, Some("r1")), ("i2", 2, Some("r2")), ("i3", 3, None)] {
            apply(&mut projection, "turn-item.updated", item(id, ordinal, run), "2026-01-01T00:00:08.500Z");
        }
        let shell = shell(&projection);
        // The run the thread is on, with the times it recorded.
        assert_eq!((shell["latestRunId"].clone(), shell["status"].clone()), (json!("r2"), json!("waiting")));
        assert_eq!((shell["latestRunRequestedAt"].clone(), shell["latestRunStartedAt"].clone(), shell["latestRunCompletedAt"].clone()), (json!("2026-01-01T00:00:01.000Z"), json!("2026-01-01T00:00:04.000Z"), Value::Null));
        // A run waiting on a person is not one that can be interrupted, but it is the thread's activity.
        assert_eq!((shell["activeRunId"].clone(), shell["activityRunStatus"].clone()), (Value::Null, json!("waiting")));
        // Wakes and agent messages use the user role too; the one a person wrote is the one that counts.
        assert_eq!((shell["latestUserMessageAt"].clone(), shell["latestUserAuthoredMessageAt"].clone()), (json!("2026-01-01T00:00:07.000Z"), json!("2026-01-01T00:00:06.000Z")));
        // The transcript stays out of the shell.
        assert_eq!(shell["latestVisibleMessage"], Value::Null);
        assert_eq!((shell["itemCount"].clone(), shell["visibleItemCount"].clone()), (json!(3), json!(3)));
        assert_eq!((shell["pullRequests"].clone(), shell["providerInstanceHistory"].clone(), shell["goal"].clone(), shell["hasActionableProposedPlan"].clone()), (json!([]), json!([]), Value::Null, json!(false)));
        for field in ["unsettledAt", "snoozedUntil", "snoozedAt", "pinnedAt", "autoSettleDisabledAt", "pinOrderKey", "titleRegeneration", "limitRecovery"] {
            assert_eq!(shell.get(field), Some(&Value::Null), "{field} is sent, null");
        }
        // The thread moved with the events.
        assert_eq!(shell["updatedAt"], "2026-01-01T00:00:08.500Z");
        assert_eq!(projection["updatedAt"], shell["updatedAt"]);
        // A rollback takes its run's items out of the count.
        let mut rolled = run("r1", 1, "rolled_back", json!("2026-01-01T00:00:02.000Z"));
        rolled["completedAt"] = json!("2026-01-01T00:00:10.000Z");
        apply(&mut projection, "run.updated", rolled, "2026-01-01T00:00:10.000Z");
        let shell = super::shell(&projection);
        assert_eq!((shell["itemCount"].clone(), shell["visibleItemCount"].clone()), (json!(2), json!(2)));
    }
}
