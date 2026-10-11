//! Thread forks and merge-backs in the orchestration core (`dispatchThreadFork`,
//! `dispatchThreadMergeBack`, `ThreadForkService`). Both are cheap: a fork is a new idle thread with
//! lineage and a pending `fork` context transfer, a merge-back is a pending `merge_back` transfer on
//! the thread the fork came from. Nothing reaches a provider until the next message to that thread,
//! which resolves the transfer (`context_transfer.rs`). Provider-agnostic: the adapters only see the
//! provider thread the first run is started on.
use crate::{execution::event, persistence::Event};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};

fn rows<'a>(value: &'a Value, field: &str) -> &'a [Value] {
    value[field].as_array().map_or(&[], Vec::as_slice)
}

fn encode(value: &str) -> String {
    crate::provider_maintenance::encode_uri_component(value)
}

/// `ContextTransferId`: type and both threads, made unique per allocation.
pub(crate) fn transfer_id(kind: &str, source: &str, target: &str) -> String {
    format!("context-transfer:type:{kind}:source-thread:{}:target-thread:{}:{}", encode(source), encode(target), uuid::Uuid::new_v4())
}

/// Provider-finished conversation, forkable whatever its outcome: a failed, interrupted or
/// cancelled turn still has a native thread or a portable transcript. In-progress and rolled-back
/// runs are not forkable (`isForkableSourceRunStatus`).
pub(crate) fn is_forkable_source_run_status(status: &str) -> bool {
    matches!(status, "completed" | "waiting" | "failed" | "interrupted" | "cancelled")
}

pub(crate) fn forkable_source_run_status_error(run: &Value) -> String {
    format!("Fork source run {} is {}; in-progress and rolled-back runs cannot be forked.", run["id"].as_str().unwrap_or(""), run["status"].as_str().unwrap_or(""))
}

/// The newest completed run that has a checkpoint (`latestStableRun`).
fn latest_stable_run(projection: &Value) -> Option<&Value> {
    rows(projection, "runs").iter().filter(|run| run["status"] == "completed" && !run["checkpointId"].is_null()).max_by_key(|run| run["ordinal"].as_u64())
}

/// The run a `latest_stable`, `run` or `checkpoint` source point names (`runForSourcePoint`).
pub(crate) fn run_for_source_point<'a>(projection: &'a Value, source_point: &Value) -> Option<&'a Value> {
    match source_point["type"].as_str()? {
        "latest_stable" => latest_stable_run(projection),
        "run" => rows(projection, "runs").iter().find(|run| run["id"] == source_point["runId"]),
        "checkpoint" => {
            let checkpoint = rows(projection, "checkpoints").iter().find(|checkpoint| checkpoint["id"] == source_point["checkpointId"])?;
            if checkpoint["runId"].is_null() { None } else { rows(projection, "runs").iter().find(|run| run["id"] == checkpoint["runId"]) }
        }
        _ => None,
    }
}

pub(crate) fn provider_thread_for_run<'a>(projection: &'a Value, run: &Value) -> Option<&'a Value> {
    if run["providerThreadId"].is_null() {
        return None;
    }
    rows(projection, "providerThreads").iter().find(|thread| thread["id"] == run["providerThreadId"])
}

/// The provider turn a run attempt is on or ended with (`latestProviderTurnForAttempt`): a Codex
/// goal keeps one run open across several native turns.
pub(crate) fn latest_provider_turn_for_attempt<'a>(provider_turns: &'a [Value], attempt_id: &Value) -> Option<&'a Value> {
    if attempt_id.is_null() {
        return None;
    }
    provider_turns.iter().filter(|turn| turn["runAttemptId"] == *attempt_id).max_by_key(|turn| turn["ordinal"].as_u64())
}

/// `providerTurnForRun`.
pub(crate) fn provider_turn_for_run<'a>(projection: &'a Value, run: &Value) -> Option<&'a Value> {
    if run["activeAttemptId"].is_null() {
        return None;
    }
    latest_provider_turn_for_attempt(rows(projection, "providerTurns"), &run["activeAttemptId"]).or_else(|| {
        let attempt = rows(projection, "attempts").iter().find(|attempt| attempt["id"] == run["activeAttemptId"])?;
        rows(projection, "providerTurns").iter().find(|turn| turn["id"] == attempt["providerTurnId"])
    })
}

/// The exact state a transfer carries (`contextSourcePointForRun`).
pub(crate) fn source_point_for_run(projection: &Value, run: &Value) -> Value {
    let mut point = json!({"threadId":projection["thread"]["id"],"runId":run["id"]});
    if !run["checkpointId"].is_null() {
        point["checkpointId"] = run["checkpointId"].clone();
    }
    if let Some(native) = provider_thread_for_run(projection, run).map(|thread| &thread["nativeThreadRef"]).filter(|native| !native.is_null()) {
        point["providerThreadRef"] = native.clone();
    }
    if let Some(native) = provider_turn_for_run(projection, run).map(|turn| &turn["nativeTurnRef"]).filter(|native| !native.is_null()) {
        point["providerTurnRef"] = native.clone();
    }
    point
}

pub(crate) fn pending_fork_transfer(projection: &Value) -> Option<&Value> {
    rows(projection, "contextTransfers").iter().find(|transfer| transfer["type"] == "fork" && transfer["targetThreadId"] == projection["thread"]["id"] && transfer["status"] == "pending")
}

pub(crate) fn pending_merge_back_transfers(projection: &Value) -> Vec<&Value> {
    rows(projection, "contextTransfers").iter().filter(|transfer| transfer["type"] == "merge_back" && transfer["targetThreadId"] == projection["thread"]["id"] && transfer["status"] == "pending").collect()
}

/// The most recently updated transfer, the last of equals (`latestContextTransfer`).
pub(crate) fn latest_context_transfer<'a>(transfers: &[&'a Value]) -> Option<&'a Value> {
    transfers.iter().copied().reduce(|latest, transfer| if transfer["updatedAt"].as_str() >= latest["updatedAt"].as_str() { transfer } else { latest })
}

/// A refusal in the shape every dispatch refusal takes.
fn refusal(command: &Value, cause: String) -> Value {
    json!({"_tag":"OrchestratorDispatchError","commandId":command["commandId"],"commandType":command["type"],"cause":cause})
}

fn source_thread<'a>(command: &Value, projection: Option<&'a Value>) -> Result<&'a Value, Value> {
    projection
        .filter(|projection| projection["thread"]["deletedAt"].is_null())
        .ok_or_else(|| refusal(command, format!("Thread {} does not exist.", command["sourceThreadId"].as_str().unwrap_or(""))))
}

/// `thread.fork`: the new thread and the pending transfer that records where it came from. The
/// command's events belong to the new thread; `source` is the thread forked from.
pub fn plan_fork(command: &Value, existing: Option<&Value>, source: Option<&Value>, now: DateTime<Utc>) -> Result<Vec<Event>, Value> {
    let source = source_thread(command, source)?;
    let target_id = command["targetThreadId"].as_str().unwrap_or("");
    if existing.is_some() {
        return Err(refusal(command, format!("Thread {target_id} already exists.")));
    }
    let source_id = source["thread"]["id"].as_str().unwrap_or("");
    let source_point = &command["sourcePoint"];
    let Some(run) = run_for_source_point(source, source_point) else {
        return Err(refusal(command, format!("No stable source run was found for fork source {}.", source_point["type"].as_str().unwrap_or(""))));
    };
    if !is_forkable_source_run_status(run["status"].as_str().unwrap_or("")) {
        return Err(refusal(command, forkable_source_run_status_error(run)));
    }
    let provider_thread = provider_thread_for_run(source, run);
    let at = crate::execution::at(now);
    let created_at = command.get("createdAt").filter(|value| !value.is_null()).cloned().unwrap_or_else(|| json!(at));
    let mut target = source["thread"].clone();
    let title = command.get("title").and_then(Value::as_str).map_or_else(|| format!("{} fork", source["thread"]["title"].as_str().unwrap_or("")), str::to_owned);
    for (key, value) in [
        ("createdBy", command["createdBy"].clone()),
        ("creationSource", command["creationSource"].clone()),
        ("id", json!(target_id)),
        ("title", json!(title)),
        ("activeProviderThreadId", Value::Null),
        ("lineage", json!({"parentThreadId":source_id,"relationshipToParent":"fork","rootThreadId":source["thread"]["lineage"]["rootThreadId"]})),
        ("forkedFrom", json!({"type":"run","threadId":source_id,"runId":run["id"]})),
        ("createdAt", created_at.clone()),
        ("updatedAt", created_at.clone()),
        ("archivedAt", Value::Null),
        ("settledOverride", Value::Null),
        ("settledAt", Value::Null),
        ("snoozedUntil", Value::Null),
        ("snoozedAt", Value::Null),
        ("lastVisitedAt", Value::Null),
        ("deletedAt", Value::Null),
    ] {
        target[key] = value;
    }
    let strong = provider_thread.is_some_and(|thread| thread["nativeThreadRef"]["strength"] == "strong");
    let transfer = json!({
        "id":transfer_id("fork", source_id, target_id),"type":"fork","sourceThreadId":source_id,"targetThreadId":target_id,
        "sourcePoint":source_point_for_run(source, run),"basePoint":null,"sourceProviderInstanceId":run["providerInstanceId"],
        "targetProviderInstanceId":null,"targetRunId":null,"status":"pending","resolution":null,"createdBy":command["createdBy"],
        "error":if strong { Value::Null } else { json!("Source provider thread does not expose a strong native thread ref.") },
        "createdAt":created_at,"updatedAt":created_at,"consumedAt":null,
    });
    let command_id = command["commandId"].as_str().unwrap_or("");
    let target = crate::execution::checked::<t3_contracts::AppThread>(target).map_err(|error| refusal(command, error.to_string()))?;
    let transfer = crate::execution::checked::<t3_contracts::ContextTransfer>(transfer).map_err(|error| refusal(command, error.to_string()))?;
    Ok(vec![
        event(target_id, command_id, "thread.created", target, now),
        event(target_id, command_id, "context-transfer.created", transfer, now),
    ])
}

/// `thread.merge_back`: the fork's work, as a pending transfer on the thread it was forked from.
/// The next message to that thread carries it (`context_transfer.rs`). A newer merge-back of the
/// same fork supersedes one still waiting.
pub fn plan_merge_back(command: &Value, target: Option<&Value>, source: Option<&Value>, now: DateTime<Utc>) -> Result<Vec<Event>, Value> {
    let source = source_thread(command, source)?;
    let (source_id, target_id) = (command["sourceThreadId"].as_str().unwrap_or(""), command["targetThreadId"].as_str().unwrap_or(""));
    let target = target
        .filter(|projection| projection["thread"]["deletedAt"].is_null())
        .ok_or_else(|| refusal(command, format!("Thread {target_id} does not exist.")))?;
    if source["thread"]["lineage"]["relationshipToParent"] != "fork" || source["thread"]["lineage"]["parentThreadId"] != json!(target_id) {
        return Err(refusal(command, format!("Thread {source_id} is not a fork of {target_id}.")));
    }
    let source_point = &command["sourcePoint"];
    let Some(run) = run_for_source_point(source, source_point) else {
        return Err(refusal(command, format!("No stable source run was found for merge-back source {}.", source_point["type"].as_str().unwrap_or(""))));
    };
    if !matches!(run["status"].as_str(), Some("completed" | "waiting")) {
        return Err(refusal(command, format!("Merge-back source run {} is {}; only provider-finished runs are supported.", run["id"].as_str().unwrap_or(""), run["status"].as_str().unwrap_or(""))));
    }
    let Some(fork_transfer) = rows(source, "contextTransfers").iter().rfind(|transfer| transfer["type"] == "fork" && transfer["sourceThreadId"] == json!(target_id) && transfer["targetThreadId"] == json!(source_id)) else {
        return Err(refusal(command, format!("No fork transfer exists between {target_id} and {source_id}.")));
    };
    let provider_thread = provider_thread_for_run(source, run);
    let at = crate::execution::at(now);
    let created_at = command.get("createdAt").filter(|value| !value.is_null()).cloned().unwrap_or_else(|| json!(at));
    let id = transfer_id("merge_back", source_id, target_id);
    let transfer = json!({
        "id":id,"type":"merge_back","sourceThreadId":source_id,"targetThreadId":target_id,
        "sourcePoint":source_point_for_run(source, run),"basePoint":fork_transfer["sourcePoint"],
        "sourceProviderInstanceId":run["providerInstanceId"],"targetProviderInstanceId":target["thread"]["modelSelection"]["instanceId"],
        "targetRunId":null,"status":"pending","resolution":null,"createdBy":command["createdBy"],
        "error":if provider_thread.is_some() { Value::Null } else { json!("Source merge-back run has no provider thread.") },
        "createdAt":created_at,"updatedAt":created_at,"consumedAt":null,
    });
    let command_id = command["commandId"].as_str().unwrap_or("");
    let mut events = vec![];
    for pending in pending_merge_back_transfers(target).into_iter().filter(|transfer| transfer["sourceThreadId"] == json!(source_id)) {
        let mut superseded = pending.clone();
        superseded["status"] = json!("superseded");
        superseded["error"] = json!(format!("Superseded by merge-back transfer {id}."));
        superseded["updatedAt"] = created_at.clone();
        events.push(event(target_id, command_id, "context-transfer.updated", superseded, now));
    }
    let transfer = crate::execution::checked::<t3_contracts::ContextTransfer>(transfer).map_err(|error| refusal(command, error.to_string()))?;
    events.push(event(target_id, command_id, "context-transfer.created", transfer, now));
    Ok(events)
}

/// `isTurnItemAtOrBeforeRun`: an item of a run no later than the fork point. An import's items
/// without a run come before every run.
fn at_or_before_run(thread: &Value, item: &Value, ordinals: &std::collections::HashMap<&str, u64>, boundary: u64) -> bool {
    match item["runId"].as_str() {
        None => thread["historyOrigin"] == "v1_import",
        Some(run) => ordinals.get(run).is_some_and(|ordinal| *ordinal <= boundary),
    }
}

/// What a fork shows (`buildVisibleTurnItems`): its source's history through the fork point
/// (including what the source itself inherited), a marker for the fork, then its own items.
/// `local` is the fork's own visible items; `source` is the source projection as stored.
pub(crate) fn visible_with_inherited(projection: &Value, source: &Value, local: Vec<Value>) -> Vec<Value> {
    let forked = &projection["thread"]["forkedFrom"];
    let Some(boundary) = rows(source, "runs").iter().find(|run| run["id"] == forked["runId"]).and_then(|run| run["ordinal"].as_u64()) else {
        return local;
    };
    let ordinals: std::collections::HashMap<&str, u64> = rows(source, "runs").iter().filter_map(|run| Some((run["id"].as_str()?, run["ordinal"].as_u64()?))).collect();
    let source_id = &source["thread"]["id"];
    // What the source inherited itself comes first: its inherited rows and its own fork marker.
    let inherited = rows(source, "visibleTurnItems").iter().filter(|row| row["item"]["threadId"] != *source_id || row["item"]["type"] == "fork").map(|row| (row["sourceThreadId"].clone(), row["sourceItemId"].clone(), row["item"].clone()));
    let mut own_items: Vec<&Value> = rows(source, "turnItems").iter().collect();
    own_items.sort_by_key(|item| item["ordinal"].as_u64());
    let own = own_items.into_iter().filter(|item| at_or_before_run(&source["thread"], item, &ordinals, boundary) && item_visible(item, rows(source, "runs"), rows(source, "attempts"), rows(source, "turnItems"))).map(|item| (item["threadId"].clone(), item["id"].clone(), item.clone()));
    let created = projection["thread"]["createdAt"].clone();
    let target_id = projection["thread"]["id"].clone();
    let marker = json!({"id":format!("turn-item:fork:{}",target_id.as_str().unwrap_or("")),"threadId":target_id,"runId":null,"nodeId":null,"providerTurnId":null,"nativeItemRef":null,"parentItemId":null,"ordinal":0,"status":"completed","title":"Forked from conversation","startedAt":null,"completedAt":created,"updatedAt":created,"type":"fork","source":{"type":"run","threadId":forked["threadId"],"runId":forked["runId"]},"targetThreadId":target_id});
    let mut visible: Vec<Value> = inherited
        .chain(own)
        .map(|(thread, item_id, item)| json!({"visibility":"inherited","sourceThreadId":thread,"sourceItemId":item_id,"item":item}))
        .collect();
    visible.push(json!({"visibility":"synthetic","sourceThreadId":forked["threadId"],"sourceItemId":marker["id"],"item":marker}));
    visible.extend(local.into_iter().map(|row| json!({"visibility":"local","sourceThreadId":row["sourceThreadId"],"sourceItemId":row["sourceItemId"],"item":row["item"]})));
    for (position, row) in visible.iter_mut().enumerate() {
        row["position"] = json!(position);
    }
    visible
}

/// A fork read now: the history it inherits is its source's as it is now, not as it was when the fork's
/// last event was reduced. The original reads a fork's visible items from its source's rows every time
/// (`buildVisibleTurnItems` in the projection read), so a command that ended or a rollback in the source
/// shows in the fork's next snapshot or page; events already delivered are not rewritten. `read` returns a
/// thread's stored projection; a source that is itself a fork is refreshed first.
pub(crate) fn refreshed(projection: Value, read: &dyn Fn(&str) -> Option<Value>) -> Value {
    refresh(projection, read, 0)
}
fn refresh(mut projection: Value, read: &dyn Fn(&str) -> Option<Value>, depth: usize) -> Value {
    if projection["thread"]["forkedFrom"]["type"] != "run" || depth > 32 {
        return projection;
    }
    let Some(source) = projection["thread"]["forkedFrom"]["threadId"].as_str().and_then(read) else { return projection };
    let source = refresh(source, read, depth + 1);
    let local: Vec<Value> = rows(&projection, "visibleTurnItems").iter().filter(|row| row["visibility"] == "local").cloned().collect();
    projection["visibleTurnItems"] = Value::Array(visible_with_inherited(&projection, &source, local));
    projection
}

/// A thread's own conversation, in the order it was written (`activeLocalTurnItems`): what
/// `item_visible` lets through, numbered from zero.
pub(crate) fn local_visible_rows(projection: &Value) -> Vec<Value> {
    let (runs, attempts, items) = (rows(projection, "runs"), rows(projection, "attempts"), rows(projection, "turnItems"));
    #[cfg(test)]
    crate::persistence::work::add(|work| work.rows_cloned += items.len() as u64);
    let statuses: std::collections::HashMap<&str, &str> = runs.iter().filter_map(|run| Some((run["id"].as_str()?, run["status"].as_str()?))).collect();
    items
        .iter()
        .filter(|item| {
            let status = item["runId"].as_str().and_then(|run| statuses.get(run)).copied();
            if status == Some("rolled_back") || (status == Some("cancelled") && item["type"] == "user_message" && item["inputIntent"] == "queued_turn") {
                return false;
            }
            item["type"] != "run_interrupt_result" || !superseded_interrupt(item, attempts, items)
        })
        .enumerate()
        .map(|(position, item)| json!({"position":position,"visibility":"local","sourceThreadId":item["threadId"],"sourceItemId":item["id"],"item":item}))
        .collect()
}

/// A stop-then-steer result whose attempt was superseded, with no request beside it, is not part
/// of the history (`isOrchestrationV2SupersededInterrupt`).
fn superseded_interrupt(item: &Value, attempts: &[Value], items: &[Value]) -> bool {
    if item["type"] != "run_interrupt_result" || item["runId"].is_null() || item["nodeId"].is_null() {
        return false;
    }
    let superseded = attempts.iter().any(|attempt| attempt["runId"] == item["runId"] && attempt["rootNodeId"] == item["nodeId"] && attempt["status"] == "superseded");
    superseded && !items.iter().any(|other| other["type"] == "run_interrupt_request" && other["runId"] == item["runId"])
}

/// `isOrchestrationV2TurnItemVisible`: rolled-back runs and the queued messages of cancelled runs
/// are not conversation.
fn item_visible(item: &Value, runs: &[Value], attempts: &[Value], items: &[Value]) -> bool {
    let status = runs.iter().find(|run| run["id"] == item["runId"]).and_then(|run| run["status"].as_str());
    if status == Some("rolled_back") || (status == Some("cancelled") && item["type"] == "user_message" && item["inputIntent"] == "queued_turn") {
        return false;
    }
    !superseded_interrupt(item, attempts, items)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread(id: &str) -> Value {
        json!({"createdBy":"user","creationSource":"web","id":id,"projectId":"project","title":"Source","providerInstanceId":"codex","modelSelection":{"instanceId":"codex","model":"gpt-5.4"},"runtimeMode":"full-access","interactionMode":"default","branch":null,"worktreePath":null,"activeProviderThreadId":"pt","lineage":{"parentThreadId":null,"relationshipToParent":null,"rootThreadId":id},"forkedFrom":null,"createdAt":"2026-01-01T00:00:00.000Z","updatedAt":"2026-01-01T00:00:00.000Z","archivedAt":null,"settledOverride":null,"settledAt":null,"snoozedUntil":null,"snoozedAt":null,"lastVisitedAt":null,"deletedAt":null})
    }

    #[test]
    fn a_latest_stable_point_needs_a_completed_run_with_a_checkpoint() {
        let projection = json!({"runs":[{"id":"a","ordinal":1,"status":"completed","checkpointId":"c1"},{"id":"b","ordinal":2,"status":"completed","checkpointId":null},{"id":"c","ordinal":3,"status":"running","checkpointId":null}]});
        assert_eq!(run_for_source_point(&projection, &json!({"type":"latest_stable"})).unwrap()["id"], "a");
        assert_eq!(run_for_source_point(&projection, &json!({"type":"run","runId":"c"})).unwrap()["status"], "running");
        assert!(run_for_source_point(&projection, &json!({"type":"checkpoint","checkpointId":"missing"})).is_none());
    }

    #[test]
    fn running_and_rolled_back_runs_are_not_forkable() {
        for status in ["completed", "waiting", "failed", "interrupted", "cancelled"] {
            assert!(is_forkable_source_run_status(status), "{status}");
        }
        for status in ["queued", "preparing", "starting", "running", "rolled_back"] {
            assert!(!is_forkable_source_run_status(status), "{status}");
        }
    }

    #[test]
    fn a_fork_inherits_the_source_through_the_fork_point_and_marks_where_it_began() {
        let mut source = json!({"thread":thread("source"),"runs":[{"id":"r1","ordinal":1},{"id":"r2","ordinal":2}],"attempts":[],"turnItems":[
            {"id":"u1","threadId":"source","runId":"r1","type":"user_message"},{"id":"u2","threadId":"source","runId":"r2","type":"user_message"}],"visibleTurnItems":[]});
        source["visibleTurnItems"] = json!([]);
        let mut fork = json!({"thread":thread("fork")});
        fork["thread"]["forkedFrom"] = json!({"type":"run","threadId":"source","runId":"r1"});
        let local = vec![json!({"sourceThreadId":"fork","sourceItemId":"own","item":{"id":"own","threadId":"fork","type":"user_message"}})];
        let visible = visible_with_inherited(&fork, &source, local);
        let shape: Vec<String> = visible.iter().map(|row| format!("{}:{}", row["visibility"].as_str().unwrap(), row["item"]["id"].as_str().unwrap())).collect();
        assert_eq!(shape, ["inherited:u1", "synthetic:turn-item:fork:fork", "local:own"]);
        assert_eq!(visible[2]["position"], 2);
    }

    #[test]
    fn a_fork_read_later_shows_its_source_as_it_is_now() {
        let mut source = json!({"thread":thread("source"),"runs":[{"id":"r1","ordinal":1,"status":"completed"}],"attempts":[],"turnItems":[
            {"id":"c1","threadId":"source","runId":"r1","type":"command_execution","status":"running","ordinal":1}],"visibleTurnItems":[]});
        let mut fork = json!({"thread":thread("fork")});
        fork["thread"]["forkedFrom"] = json!({"type":"run","threadId":"source","runId":"r1"});
        fork["visibleTurnItems"] = json!(visible_with_inherited(&fork, &source, vec![json!({"sourceThreadId":"fork","sourceItemId":"own","item":{"id":"own","threadId":"fork","type":"user_message"}})]));
        assert_eq!(fork["visibleTurnItems"][0]["item"]["status"], "running");
        // The source's command ends after the fork was made, and its run is rolled back to nothing later.
        source["turnItems"][0]["status"] = json!("completed");
        let sources = std::collections::HashMap::from([("source".to_owned(), source)]);
        let now = refreshed(fork.clone(), &|id| sources.get(id).cloned());
        let shape: Vec<String> = now["visibleTurnItems"].as_array().unwrap().iter().map(|row| format!("{}:{}:{}", row["visibility"].as_str().unwrap(), row["item"]["id"].as_str().unwrap(), row["item"]["status"].as_str().unwrap_or(""))).collect();
        assert_eq!(shape, ["inherited:c1:completed", "synthetic:turn-item:fork:fork:completed", "local:own:"]);
        // A thread that is not a fork, or whose source is gone, is read as stored.
        assert_eq!(refreshed(fork.clone(), &|_| None), fork);
        let plain = json!({"thread":thread("plain"),"visibleTurnItems":[]});
        assert_eq!(refreshed(plain.clone(), &|_| panic!("a plain thread reads nothing")), plain);
    }
}
