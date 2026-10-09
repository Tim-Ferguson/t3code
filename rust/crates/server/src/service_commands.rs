//! The nine `orchestration.dispatchCommand` commands the server's own services issue and that the
//! original also accepts from a client: `ws.ts` hands every command of its public union to the same
//! orchestrator, which has no sender policy, so over RPC they run through the same code the server uses
//! internally. A delegated task request is stamped `createdBy: "user"` first (`withCreationProvenance`).
//!
//! `delegated_task.*` and `thread.created.record` are `DelegatedTasks`; the detach of a provider session
//! and the three thread mutations (`thread.pull-request-link.sync`, `thread.pull-request.sync`,
//! `thread.title.regeneration.complete`) are planned here against the thread projection.
use crate::{
    delegated_completion::DelegatedTasks,
    execution::{ExecutionService, at, checked, event},
    persistence::{Decision, Event, NewEffect, Receipt, Store, StoreError, read_projection},
    thread, thread_pull_requests as links,
};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};

const KINDS: [&str; 9] = [
    "delegated_task.request",
    "delegated_task.wake-policy",
    "delegated_task.completion-delivery.acknowledge",
    "delegated_task.completion-delivery.dispose",
    "provider-session.detach",
    "thread.created.record",
    "thread.pull-request-link.sync",
    "thread.pull-request.sync",
    "thread.title.regeneration.complete",
];

/// Whether `orchestration.dispatchCommand` runs this command through this module.
pub(crate) fn is_service_command(kind: &str) -> bool {
    KINDS.contains(&kind)
}

/// The command as the service reads it. The payload gate already holds it to the original's schema.
pub(crate) fn command(payload: &Value) -> Result<Value, String> {
    let mut input = payload.clone();
    if !input.is_object() {
        return Err("command must be an object".into());
    }
    if input["type"] == "delegated_task.request" {
        input["createdBy"] = json!("user");
        if input.get("creationSource").is_none() {
            input["creationSource"] = json!("web");
        }
    }
    Ok(input)
}

fn rows<'a>(value: &'a Value, field: &str) -> &'a [Value] {
    value[field].as_array().map_or(&[], Vec::as_slice)
}

fn rejection(command: &Value, cause: String) -> Value {
    json!({"_tag":"OrchestratorDispatchError","commandId":command["commandId"],"commandType":command["type"],"cause":cause})
}

/// Runs one of the nine. `execution` is absent on a server without providers, which the delegated
/// commands need.
pub(crate) fn dispatch(store: &Store, execution: Option<&ExecutionService>, payload: &Value, now: DateTime<Utc>) -> Result<Option<Receipt>, StoreError> {
    let command = command(payload).map_err(StoreError::InvalidCommand)?;
    let kind = command["type"].as_str().unwrap_or_default();
    if kind.starts_with("delegated_task.") || kind == "thread.created.record" {
        let Some(execution) = execution else { return Ok(None) };
        let tasks = DelegatedTasks::new(execution.clone());
        return match kind {
            "delegated_task.request" => tasks.request(&command, now),
            "delegated_task.wake-policy" => tasks.wake_policy(&command, now),
            "thread.created.record" => tasks.record_created_thread(&command, now),
            _ => tasks.resolve_delivery(&command, now),
        }
        .map(Some);
    }
    let (command_id, thread_id) = (command["commandId"].as_str().unwrap_or_default().to_owned(), command["threadId"].as_str().unwrap_or_default().to_owned());
    store
        .dispatch(&command_id, "thread", &thread_id, kind, now, |transaction| {
            let Some(projection) = read_projection(transaction, "thread", &thread_id)? else {
                return Ok(Decision::Rejected(rejection(&command, format!("Thread {thread_id} does not exist."))));
            };
            let planned = if kind == "provider-session.detach" {
                plan_detach(&command, &projection, now)
            } else {
                let project = match kind {
                    "thread.pull-request.sync" => read_projection(transaction, "project", command["projectId"].as_str().unwrap_or_default())?,
                    _ => None,
                };
                let sequence: u64 = transaction.query_row("SELECT COALESCE(MAX(sequence),0) FROM rust_application_events WHERE aggregate_kind='thread' AND aggregate_id=?1", [&thread_id], |row| row.get(0))?;
                plan_thread_mutation(&command, &projection, project.as_ref(), sequence, now)
            };
            Ok(match planned {
                Ok((events, effects)) => Decision::Accepted { events, effects },
                Err(error) => Decision::Rejected(error),
            })
        }, thread::reduce)
        .map(Some)
}

type Planned = (Vec<Event>, Vec<NewEffect>);

/// `dispatchProviderSessionDetach`: the session is released on the provider, whatever state it is in.
fn plan_detach(command: &Value, projection: &Value, now: DateTime<Utc>) -> Result<Planned, Value> {
    let (thread_id, command_id, session_id) = (command["threadId"].as_str().unwrap_or_default(), command["commandId"].as_str().unwrap_or_default(), command["providerSessionId"].as_str().unwrap_or_default());
    if !rows(projection, "providerSessions").iter().any(|session| session["id"] == session_id) {
        return Err(rejection(command, format!("Provider session {session_id} does not belong to thread {thread_id}.")));
    }
    let mut payload = json!({"providerSessionId":session_id,"detachedAt":at(now)});
    let mut request = json!({"type":"provider-session.detach","providerSessionId":session_id});
    if let Some(reason) = command.get("reason").filter(|reason| !reason.is_null()) {
        payload["reason"] = reason.clone();
        request["detail"] = reason.clone();
    }
    let detached = Event {
        event_id: uuid::Uuid::new_v4().to_string(),
        aggregate_kind: "thread".into(),
        aggregate_id: thread_id.into(),
        occurred_at: at(now),
        command_id: Some(command_id.into()),
        causation_event_id: None,
        correlation_id: Some(command_id.into()),
        event_type: "provider-session.detached".into(),
        payload,
        metadata: json!({}),
    };
    let effect = NewEffect { id: format!("effect:{command_id}:provider-session.detach:{session_id}"), command_id: command_id.into(), thread_id: thread_id.into(), request, available_at: at(now) };
    Ok((vec![detached], vec![effect]))
}

/// The thread mutations of `dispatchThreadMutation` that record what the server read: a link's snapshot
/// and stack, the pull requests of the thread's branch, and a finished title regeneration. Each announces
/// the thread whether or not it changed.
pub(crate) fn plan_thread_mutation(command: &Value, projection: &Value, project: Option<&Value>, latest_sequence: u64, now: DateTime<Utc>) -> Result<Planned, Value> {
    let (thread_id, command_id, kind) = (command["threadId"].as_str().unwrap_or_default(), command["commandId"].as_str().unwrap_or_default(), command["type"].as_str().unwrap_or_default());
    let reject = |cause: String| rejection(command, cause);
    let mut thread = projection["thread"].clone();
    if !thread["deletedAt"].is_null() {
        return Err(reject(format!("Thread {thread_id} is deleted.")));
    }
    if kind == "thread.pull-request.sync" {
        if !thread["archivedAt"].is_null() {
            return Err(reject(format!("Thread {thread_id} is archived.")));
        }
        let expected = &command["expected"];
        let same = |left: &Value, right: &Value| (left.is_null() && right.is_null()) || left == right;
        let stale = project.is_none_or(|project| !project["deletedAt"].is_null() || project["workspaceRoot"] != expected["workspaceRoot"])
            || latest_sequence > command["snapshotSequence"].as_u64().unwrap_or(0)
            || thread["projectId"] != command["projectId"]
            || !same(&thread["branch"], &expected["branch"])
            || !same(&thread["worktreePath"], &expected["worktreePath"])
            || !same(&thread["linkedPullRequest"], &expected["linkedPullRequest"])
            || !same(&thread["branchPullRequest"], &expected["branchPullRequest"]);
        if stale {
            return Err(reject(format!("Thread {thread_id} changed before pull request discovery.")));
        }
    }
    let stamp = at(now);
    match kind {
        "thread.pull-request-link.sync" => {
            let key = links::normalize(command);
            let current = links::links(&thread);
            let same_key = |link: &Value| links::normalize(link) == key;
            if current.iter().any(same_key) {
                let pull_requests: Vec<Value> = current.iter().map(|link| if same_key(link) { let mut link = link.clone(); link["snapshot"] = command["snapshot"].clone(); link["stack"] = command["stack"].clone(); link } else { link.clone() }).collect();
                let linked_stays = !thread["linkedPullRequest"].is_null() && pull_requests.iter().any(|link| link["source"] != "stack-dismissed" && links::normalize(link) == links::normalize(&links::legacy_key(&thread["linkedPullRequest"], None)));
                if !linked_stays {
                    thread["linkedPullRequest"] = Value::Null;
                }
                thread["pullRequests"] = json!(pull_requests);
            }
        }
        "thread.pull-request.sync" => {
            thread["branchPullRequest"] = command["branchPullRequest"].clone();
            if let Some(linked) = command.get("linkedPullRequest").filter(|linked| !linked.is_null()) {
                links::update_legacy(&mut thread, linked, &stamp);
            }
        }
        _ => {
            if thread["titleRegeneration"]["requestId"] == command["requestId"] {
                if let Some(title) = command.get("title").filter(|title| !title.is_null()) {
                    thread["title"] = title.clone();
                }
                thread["titleRegeneration"] = Value::Null;
                thread["updatedAt"] = json!(stamp);
            }
        }
    }
    let thread = checked::<t3_contracts::AppThread>(thread).map_err(|error| reject(error.to_string()))?;
    let event_type = if kind == "thread.title.regeneration.complete" { "thread.metadata-updated" } else { "thread.pull-request-synced" };
    Ok((vec![event(thread_id, command_id, event_type, thread, now)], vec![]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn projection() -> Value {
        let link = |number: u64| json!({"host":"github.com","repository":"o/r","number":number,"url":format!("https://github.com/o/r/pull/{number}"),"source":"manual","linkedAt":"2026-01-01T00:00:00.000Z","snapshot":null,"stack":null});
        json!({"thread":{"id":"t","projectId":"p","title":"Title","providerInstanceId":"codex","modelSelection":{"instanceId":"codex","model":"m"},"runtimeMode":"full-access","interactionMode":"default","branch":null,"worktreePath":null,"activeProviderThreadId":null,
            "lineage":{"parentThreadId":null,"relationshipToParent":null,"rootThreadId":"t"},"forkedFrom":null,"createdAt":"2026-01-01T00:00:00.000Z","updatedAt":"2026-01-01T00:00:00.000Z","archivedAt":null,"settledOverride":null,"settledAt":null,"snoozedUntil":null,"snoozedAt":null,"lastVisitedAt":null,"deletedAt":null,
            "createdBy":"user","creationSource":"web","pullRequests":[link(1),link(2)],"titleRegeneration":{"requestId":"r1","startedAt":"2026-01-01T00:00:00.000Z"}},"providerSessions":[{"id":"s1"}]})
    }

    #[test]
    fn a_client_delegated_task_request_is_stamped_as_the_user() {
        let stamped = command(&json!({"type":"delegated_task.request","createdBy":"agent","creationSource":"mcp","commandId":"c"})).unwrap();
        assert_eq!((stamped["createdBy"].as_str(), stamped["creationSource"].as_str()), (Some("user"), Some("mcp")));
        assert_eq!(command(&json!({"type":"delegated_task.request","commandId":"c"})).unwrap()["creationSource"], "web");
    }

    #[test]
    fn a_link_sync_fills_only_the_named_links_snapshot_and_leaves_the_thread_unchanged_when_unlinked() {
        let snapshot = json!({"state":"open","title":"T","headBranch":"h","baseBranch":"main","isDraft":false,"updatedAt":"2026-02-01T00:00:00.000Z","syncedAt":"2026-02-01T00:00:00.000Z"});
        let command = json!({"type":"thread.pull-request-link.sync","commandId":"c","threadId":"t","host":"GitHub.com","repository":"O/R","number":2,"snapshot":snapshot,"stack":null});
        let (events, _) = plan_thread_mutation(&command, &projection(), None, 0, Utc::now()).unwrap();
        let pulls = &events[0].payload["pullRequests"];
        assert_eq!((pulls[0]["snapshot"].is_null(), pulls[1]["snapshot"]["state"].as_str()), (true, Some("open")));
        assert_eq!(events[0].payload["updatedAt"], "2026-01-01T00:00:00.000Z");
        let missing = json!({"type":"thread.pull-request-link.sync","commandId":"c","threadId":"t","host":"github.com","repository":"o/r","number":9,"snapshot":snapshot,"stack":null});
        let (events, _) = plan_thread_mutation(&missing, &projection(), None, 0, Utc::now()).unwrap();
        assert!(events[0].payload["pullRequests"].as_array().unwrap().iter().all(|link| link["snapshot"].is_null()));
    }

    #[test]
    fn a_pull_request_discovery_is_refused_once_the_thread_moved_on() {
        let project = json!({"deletedAt":null,"workspaceRoot":"/w"});
        let command = json!({"type":"thread.pull-request.sync","commandId":"c","threadId":"t","projectId":"p","snapshotSequence":5,"expected":{"workspaceRoot":"/w","branch":null,"worktreePath":null,"linkedPullRequest":null,"branchPullRequest":null},"branchPullRequest":{"projectId":"p","number":3,"url":"https://github.com/o/r/pull/3","repository":"o/r"}});
        let (events, _) = plan_thread_mutation(&command, &projection(), Some(&project), 5, Utc::now()).unwrap();
        assert_eq!(events[0].payload["branchPullRequest"]["number"], 3);
        for (sequence, project) in [(6, Some(&project)), (5, None)] {
            let error = plan_thread_mutation(&command, &projection(), project, sequence, Utc::now()).unwrap_err();
            assert_eq!(error["cause"], "Thread t changed before pull request discovery.");
        }
    }

    #[test]
    fn a_finished_title_regeneration_lands_only_while_its_request_is_current() {
        let mut command = json!({"type":"thread.title.regeneration.complete","commandId":"c","threadId":"t","requestId":"r1","title":"Better"});
        let (events, _) = plan_thread_mutation(&command, &projection(), None, 0, Utc::now()).unwrap();
        assert_eq!((events[0].payload["title"].as_str(), events[0].payload["titleRegeneration"].is_null()), (Some("Better"), true));
        command["requestId"] = json!("other");
        let (events, _) = plan_thread_mutation(&command, &projection(), None, 0, Utc::now()).unwrap();
        assert_eq!((events[0].payload["title"].as_str(), events[0].payload["titleRegeneration"]["requestId"].as_str()), (Some("Title"), Some("r1")));
    }

    #[test]
    fn detaching_a_session_the_thread_does_not_have_is_refused() {
        let command = json!({"type":"provider-session.detach","commandId":"c","threadId":"t","providerSessionId":"s2"});
        assert_eq!(plan_detach(&command, &projection(), Utc::now()).unwrap_err()["cause"], "Provider session s2 does not belong to thread t.");
        let ok = json!({"type":"provider-session.detach","commandId":"c","threadId":"t","providerSessionId":"s1","reason":"client-requested"});
        let (events, effects) = plan_detach(&ok, &projection(), Utc::now()).unwrap();
        assert_eq!((events[0].payload["reason"].as_str(), effects[0].request["detail"].as_str()), (Some("client-requested"), Some("client-requested")));
    }
}
