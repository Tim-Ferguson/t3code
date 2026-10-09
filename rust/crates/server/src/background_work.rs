//! Work that outlives the turn which started it: commands, dynamic tools and subagents still active
//! after their run settled, a provider's roster of background tasks, and pull request watches. The
//! thread shell lists it as pending background work; Stop ends exactly what is listed. Port of
//! `derivePendingBackgroundWork` and the Stop settlement in `Orchestrator.settleBackgroundWork`.
use crate::persistence::{Decision, Effect, Store, StoreError, read_projection};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::collections::HashSet;

const BACKGROUND_ITEM_TYPES: [&str; 3] = ["command_execution", "dynamic_tool", "subagent"];

fn rows<'a>(value: &'a Value, field: &str) -> &'a [Value] {
    value[field].as_array().map_or(&[], Vec::as_slice)
}

pub(crate) fn is_background_type(kind: &str) -> bool {
    BACKGROUND_ITEM_TYPES.contains(&kind)
}

/// `pending`, `running` and `waiting` work keeps a turn alive; idle work is resumable and does not.
pub(crate) fn work_active(status: &Value) -> bool {
    matches!(status.as_str(), Some("pending" | "running" | "waiting"))
}

/// The latest run that stands for the thread's outcome: a held queue waits for the user, so its
/// runs never do (`latestUnheldRun`).
pub(crate) fn latest_unheld_run(projection: &Value) -> Option<&Value> {
    let mut latest: Option<&Value> = None;
    for run in rows(projection, "runs") {
        if run["status"] == "queued" && run["queueHeld"] == true {
            continue;
        }
        if latest.is_none_or(|current| run["ordinal"].as_u64() > current["ordinal"].as_u64()) {
            latest = Some(run);
        }
    }
    latest
}

/// Whether a turn-item update can end background work a settled run is still waiting on: an item
/// of a background type that is no longer active.
pub(crate) fn update_can_end_background_work(item: &Value) -> bool {
    item["type"].as_str().is_some_and(|kind| BACKGROUND_ITEM_TYPES.contains(&kind)) && !work_active(&item["status"])
}

/// A dynamic tool its input marks as meant to outlive the turn that started it.
pub(crate) fn is_persistent_dynamic_tool(item: &Value) -> bool {
    persistent_dynamic_tool(item)
}
fn persistent_dynamic_tool(item: &Value) -> bool {
    item["type"] == "dynamic_tool" && item["input"].as_object().is_some_and(|input| input.get("persistent") == Some(&Value::Bool(true)))
}

/// The turn items the pending-work list names, without its settled-run gate. Stop ends exactly
/// these, so the list and Stop cannot disagree.
pub(crate) fn pending_items<'a>(turn_items: &'a [Value], runs: &[Value]) -> Vec<&'a Value> {
    let rolled_back: HashSet<&str> = runs.iter().filter(|run| run["status"] == "rolled_back").filter_map(|run| run["id"].as_str()).collect();
    turn_items
        .iter()
        .filter(|item| {
            item["type"].as_str().is_some_and(|kind| BACKGROUND_ITEM_TYPES.contains(&kind))
                && work_active(&item["status"])
                && !persistent_dynamic_tool(item)
                && item["runId"].as_str().is_none_or(|run_id| !rolled_back.contains(run_id))
        })
        .collect()
}

fn description(item: &Value) -> Option<String> {
    let text = |value: &Value| value.as_str().map(str::trim).filter(|text| !text.is_empty()).map(str::to_owned);
    text(&item["title"]).or_else(|| match item["type"].as_str() {
        Some("command_execution") => text(&item["input"]),
        Some("dynamic_tool") => text(&item["toolName"]),
        Some("subagent") => text(&item["prompt"]),
        _ => None,
    })
}

fn task_of(item: &Value, task_id: &str) -> Value {
    let mut task = json!({"taskId":task_id});
    if let Some(text) = description(item) {
        task["description"] = json!(text);
    }
    match item["type"].as_str() {
        Some("subagent") => {
            task["kind"] = json!("subagent");
            if let Some(child) = item["childThreadId"].as_str() {
                task["childThreadId"] = json!(child);
            }
        }
        Some("command_execution") => task["kind"] = json!("command"),
        _ => task["kind"] = json!("background_task"),
    }
    task
}

fn native_task_id(item: &Value) -> String {
    item["nativeItemRef"]["nativeId"].as_str().filter(|id| !id.is_empty()).or_else(|| item["id"].as_str()).unwrap_or_default().to_owned()
}

fn watch_tasks(pull_requests: &Value) -> Vec<Value> {
    pull_requests
        .as_array()
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .filter(|link| !link["watch"].is_null() && link.get("watch").is_some() && link["source"] != "stack-dismissed")
        .map(|link| {
            let key = crate::thread_pull_requests::normalize(link);
            json!({"taskId":format!("pull-request-watch:{}/{}#{}",key["host"].as_str().unwrap_or_default(),key["repository"].as_str().unwrap_or_default(),key["number"]),"description":format!("Watching pull request #{}",link["number"]),"kind":"monitor"})
        })
        .collect()
}

pub(crate) struct Derivation<'a> {
    pub latest_run: Option<&'a Value>,
    pub provider_threads: &'a [Value],
    pub turn_items: &'a [Value],
    pub active_provider_thread_id: Option<&'a str>,
    pub runs: &'a [Value],
    pub pull_requests: &'a Value,
}

/// The normalized pending-work list: gated on the latest run having settled, empty while a
/// foreground run is active, deduplicated by native task id, without persistent monitors or work of
/// a rolled-back run.
pub(crate) fn derive(input: &Derivation) -> Vec<Value> {
    let has_active_run = input.runs.iter().any(|run| matches!(run["status"].as_str(), Some("preparing" | "starting" | "running")));
    if has_active_run {
        return vec![];
    }
    let Some(latest) = input.latest_run else { return watch_tasks(input.pull_requests) };
    if !matches!(latest["status"].as_str(), Some("cancelled" | "completed" | "failed" | "interrupted" | "waiting")) {
        return vec![];
    }
    let mut tasks: Vec<(String, Value)> = vec![];
    let mut add = |id: String, task: Value, replace: bool| match tasks.iter_mut().find(|(known, _)| *known == id) {
        Some(known) if replace => known.1 = task,
        Some(_) => {}
        None => tasks.push((id, task)),
    };
    for thread in input.provider_threads.iter().filter(|thread| input.active_provider_thread_id.is_none_or(|active| thread["id"] == active)) {
        for task in thread["pendingBackgroundTasks"].as_array().map_or(&[][..], Vec::as_slice) {
            let Some(id) = task["taskId"].as_str().filter(|id| !id.is_empty()) else { continue };
            let mut task = task.clone();
            match task["description"].as_str().map(str::trim) {
                Some(text) if !text.is_empty() => task["description"] = json!(text),
                _ => {
                    task.as_object_mut().map(|object| object.remove("description"));
                }
            }
            add(id.to_owned(), task, false);
        }
    }
    for item in pending_items(input.turn_items, input.runs) {
        let id = native_task_id(item);
        let task = task_of(item, &id);
        add(id, task, false);
    }
    for task in watch_tasks(input.pull_requests) {
        add(task["taskId"].as_str().unwrap_or_default().to_owned(), task, true);
    }
    tasks.into_iter().map(|(_, task)| task).collect()
}

/// Pending work of a whole thread projection, as the thread shell lists it.
pub(crate) fn derive_for(projection: &Value) -> Vec<Value> {
    derive(&Derivation {
        latest_run: latest_unheld_run(projection),
        provider_threads: rows(projection, "providerThreads"),
        turn_items: rows(projection, "turnItems"),
        active_provider_thread_id: projection["thread"]["activeProviderThreadId"].as_str(),
        runs: rows(projection, "runs"),
        pull_requests: &projection["thread"]["pullRequests"],
    })
}

/// Whether Stop on this run reaches background work: only the latest executed run owns it.
pub(crate) fn run_owns_pending_work(projection: &Value, run: &Value) -> bool {
    let latest = rows(projection, "runs").iter().rfind(|candidate| candidate["status"] != "queued");
    latest.is_some_and(|latest| latest["id"] == run["id"])
        && derive(&Derivation {
            latest_run: Some(run),
            provider_threads: rows(projection, "providerThreads"),
            turn_items: rows(projection, "turnItems"),
            active_provider_thread_id: projection["thread"]["activeProviderThreadId"].as_str(),
            runs: rows(projection, "runs"),
            // Stop ends pull request watches on its own.
            pull_requests: &Value::Null,
        })
        .len()
            > 0
}

/// What Stop leaves of work no process will report on again: every pending background item of the
/// stopped run and the runs before it ends as interrupted, and a roster nobody owns is cleared.
/// `live_sessions` are the provider sessions whose process still owns its roster.
pub(crate) fn settle(projection: &Value, through_run_ordinal: u64, live_sessions: &HashSet<String>, completed_at: &str) -> Vec<(&'static str, Value)> {
    let runs = rows(projection, "runs");
    let mut events = vec![];
    for item in pending_items(rows(projection, "turnItems"), runs) {
        // An item without a run counts as the stopped run's.
        let ordinal = item["runId"].as_str().and_then(|id| runs.iter().find(|run| run["id"] == id)).and_then(|run| run["ordinal"].as_u64());
        if ordinal.is_some_and(|ordinal| ordinal > through_run_ordinal) {
            continue;
        }
        let mut item = item.clone();
        item["status"] = json!("interrupted");
        item["completedAt"] = json!(completed_at);
        item["updatedAt"] = json!(completed_at);
        events.push(("turn-item.updated", item));
    }
    for thread in rows(projection, "providerThreads") {
        let unowned = thread["providerSessionId"].as_str().is_none_or(|session| !live_sessions.contains(session));
        if thread["pendingBackgroundTasks"].as_array().is_none_or(Vec::is_empty) || !unowned {
            continue;
        }
        let mut thread = thread.clone();
        if thread["status"] == "active" && thread["lastRunOrdinal"].as_u64().is_some_and(|last| last <= through_run_ordinal) {
            thread["status"] = json!("idle");
        }
        thread["pendingBackgroundTasks"] = json!([]);
        thread["updatedAt"] = json!(completed_at);
        events.push(("provider-thread.updated", thread));
    }
    events
}

/// Stop on a run that already settled but whose work is still pending: the run is not interrupted
/// again; the stop is recorded, the queue held on request, and the work ended by an effect once the
/// provider has been told to stop it (`stop`).
pub(crate) fn plan_stop(command: &Value, projection: &Value, run: &Value, now: DateTime<Utc>) -> Result<Decision, StoreError> {
    if !run_owns_pending_work(projection, run) {
        return Err(StoreError::InvalidCommand("Run is no longer active.".into()));
    }
    let thread_id = command["threadId"].as_str().unwrap_or_default();
    let command_id = command["commandId"].as_str().unwrap_or_default();
    let timestamp = crate::execution::at(now);
    // A failed start has no provider turn: the work belongs to the provider thread's latest one.
    let turn = rows(projection, "providerTurns").iter().rfind(|turn| turn["providerThreadId"] == run["providerThreadId"]);
    let ordinal = rows(projection, "turnItems").iter().filter_map(|item| item["ordinal"].as_u64()).max().map_or(0, |ordinal| ordinal + 1);
    let item = json!({"id":crate::execution::run_signal_item_id(run["id"].as_str().unwrap_or_default(),"interrupt-request"),"threadId":thread_id,"runId":run["id"],"nodeId":run["rootNodeId"],"providerThreadId":run["providerThreadId"],"providerTurnId":turn.map_or(Value::Null,|turn|turn["id"].clone()),"nativeItemRef":null,"parentItemId":null,"ordinal":ordinal,"status":"completed","title":"Interrupt requested","startedAt":timestamp,"completedAt":timestamp,"updatedAt":timestamp,"type":"run_interrupt_request","message":command.get("reason").cloned().unwrap_or(json!("Interrupt requested"))});
    let mut events = vec![crate::execution::event(thread_id, command_id, "turn-item.updated", crate::execution::checked::<t3_contracts::TurnItem>(item)?, now)];
    if command["holdQueue"] == true {
        for queued in rows(projection, "runs").iter().filter(|queued| queued["status"] == "queued" && queued["queueHeld"] != true) {
            let mut queued = queued.clone();
            queued["queueHeld"] = json!(true);
            events.push(crate::execution::event(thread_id, command_id, "run.updated", crate::execution::checked::<t3_contracts::Run>(queued)?, now));
        }
    }
    events.extend(crate::delegated_tasks::stop_remaining(command, projection, run, now)?);
    let effect = crate::execution::effect(command, json!({"type":"background-work.stop","runId":run["id"],"throughRunOrdinal":run["ordinal"]}), now);
    Ok(Decision::Accepted { events, effects: vec![effect] })
}

/// The `background-work.stop` effect: whatever the thread still shows as running work of the stopped
/// run or an earlier one is work no process will report on, so Stop ends it. The settlement commits
/// under the effect's own command id, so a retried effect finds it instead of settling work that
/// appeared since.
pub(crate) fn stop(store: &Store, effect: &Effect, live_sessions: &HashSet<String>) -> Result<(), StoreError> {
    let command_id = format!("{}:background-work-settled", effect.id);
    let through = effect.request["throughRunOrdinal"].as_u64().unwrap_or(0);
    let now = Utc::now();
    store.dispatch(
        &command_id,
        "thread",
        &effect.thread_id,
        "thread.background-work.settle",
        now,
        |transaction| {
            let projection = read_projection(transaction, "thread", &effect.thread_id)?.ok_or_else(|| StoreError::InvalidCommand("Thread not found.".into()))?;
            // A new turn may have started since Stop; its work is not this Stop's.
            if rows(&projection, "runs").iter().any(|run| matches!(run["status"].as_str(), Some("preparing" | "starting" | "running"))) {
                return Ok(Decision::Accepted { events: vec![], effects: vec![] });
            }
            let settled = settle(&projection, through, live_sessions, &crate::execution::at(now));
            let events = settled
                .into_iter()
                .map(|(kind, payload)| {
                    let payload = t3_contracts::normalize_event_payload(kind, payload).map_err(|error| StoreError::InvalidCommand(error.to_string()))?;
                    Ok(crate::execution::event(&effect.thread_id, &command_id, kind, payload, now))
                })
                .collect::<Result<Vec<_>, StoreError>>()?;
            Ok(Decision::Accepted { events, effects: vec![] })
        },
        crate::thread::reduce,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pending-work list and the items Stop ends agree with the original on scripted threads.
    #[test]
    fn derivation_and_settlement_match_the_original() {
        let mut compared = 0;
        for line in include_str!("../tests/fixtures/background-work.jsonl").lines() {
            let row: Value = serde_json::from_str(line).unwrap();
            let name = row["name"].as_str().unwrap();
            let input = &row["input"];
            let latest = (!input["latestRun"].is_null()).then(|| &input["latestRun"]);
            let derived = derive(&Derivation {
                latest_run: latest,
                provider_threads: rows(input, "providerThreads"),
                turn_items: rows(input, "turnItems"),
                active_provider_thread_id: input["activeProviderThreadId"].as_str(),
                runs: rows(input, "runs"),
                pull_requests: &input["pullRequests"],
            });
            assert_eq!(Value::Array(derived), row["tasks"], "{name}");
            let pending: Vec<&str> = pending_items(rows(input, "turnItems"), rows(input, "runs")).iter().filter_map(|item| item["id"].as_str()).collect();
            assert_eq!(json!(pending), row["pendingItemIds"], "{name}");
            compared += 1;
        }
        assert_eq!(compared, include_str!("../tests/fixtures/background-work.jsonl").lines().count());
        assert!(compared >= 12);
    }

    #[test]
    fn settling_ends_pending_items_through_the_stopped_run_and_unowned_rosters() {
        let projection = json!({
            "runs":[{"id":"run-1","ordinal":1,"status":"failed"},{"id":"run-2","ordinal":2,"status":"failed"}],
            "turnItems":[
                {"id":"a","type":"command_execution","status":"running","runId":"run-1"},
                {"id":"b","type":"command_execution","status":"running","runId":"run-2"},
                {"id":"c","type":"assistant_message","status":"running","runId":"run-1"}],
            "providerThreads":[{"id":"pt","status":"active","lastRunOrdinal":1,"providerSessionId":"dead","pendingBackgroundTasks":[{"taskId":"t","kind":"command"}]}]});
        let events = settle(&projection, 1, &HashSet::new(), "2026-01-01T00:00:00.000Z");
        let ids: Vec<(&str, &Value)> = events.iter().map(|(kind, payload)| (*kind, &payload["id"])).collect();
        assert_eq!(ids, vec![("turn-item.updated", &json!("a")), ("provider-thread.updated", &json!("pt"))]);
        assert_eq!(events[1].1["status"], "idle");
        let owned = settle(&projection, 1, &HashSet::from(["dead".to_owned()]), "2026-01-01T00:00:00.000Z");
        assert_eq!(owned.len(), 1);
    }
}
