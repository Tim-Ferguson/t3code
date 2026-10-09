//! The `checkpoint.capture` outbox effect (port of `CheckpointCaptureService.ts` and the
//! checkpoint half of `RunFinalizationService.ts`).
//!
//! A provider turn that ends `completed` leaves its run `waiting`; this effect then records
//! the turn's checkpoint and completes the run. Stopped runs capture too: their checkpoint is
//! the rollback point for the next message, but their status stays as Stop left it.
use crate::{
    checkpoint_service::{CheckpointGit, CheckpointService, encode_part, shared},
    execution::{at, event},
    persistence::{Decision, Effect, NewEffect, Store, StoreError, read_projection},
    thread,
};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};

fn rows<'a>(projection: &'a Value, field: &str) -> &'a [Value] {
    projection[field]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
}
fn by_id<'a>(projection: &'a Value, field: &str, id: &Value) -> Option<&'a Value> {
    rows(projection, field).iter().find(|row| row["id"] == *id)
}

pub(crate) enum Plan {
    /// An earlier execution already committed the result, or a rollback discarded the run.
    Skip,
    Events(Vec<(&'static str, Value)>),
}

fn is_stopped(run: &Value) -> bool {
    matches!(run["status"].as_str(), Some("interrupted" | "cancelled"))
}

/// Decide the events of one capture. `Err` is the source's `CheckpointCaptureExecutionError`
/// cause: the persisted target is incomplete or no longer waiting, so the effect retries.
pub(crate) async fn plan_capture<G: CheckpointGit>(
    service: &CheckpointService<G>,
    projection: &Value,
    run_id: &str,
    scope_id: &str,
    captured_at: DateTime<Utc>,
) -> Result<Plan, String> {
    let run = by_id(projection, "runs", &json!(run_id));
    let stopped = run.is_some_and(is_stopped);
    if let Some(run) = run {
        if !run["checkpointId"].is_null()
            && (run["status"] == "completed" || stopped)
        {
            return Ok(Plan::Skip);
        }
        if run["status"] == "rolled_back" {
            return Ok(Plan::Skip);
        }
    }
    let incomplete = || "The persisted checkpoint capture target is incomplete or no longer waiting.".to_owned();
    let run = run.ok_or_else(incomplete)?;
    let root = by_id(projection, "nodes", &run["rootNodeId"]).ok_or_else(incomplete)?;
    let scope = by_id(projection, "checkpointScopes", &json!(scope_id)).ok_or_else(incomplete)?;
    let provider_thread =
        by_id(projection, "providerThreads", &run["providerThreadId"]).ok_or_else(incomplete)?;
    if (run["status"] != "waiting" && !stopped) || root["checkpointScopeId"] != scope["id"] {
        return Err(incomplete());
    }
    let ready: Vec<u64> = rows(projection, "checkpoints")
        .iter()
        .filter(|checkpoint| checkpoint["scopeId"] == scope["id"] && checkpoint["status"] == "ready")
        .filter_map(|checkpoint| checkpoint["ordinalWithinScope"].as_u64())
        .collect();
    let ordinal = run["ordinal"].as_u64().ok_or_else(incomplete)?;
    let baseline_ordinal = ordinal.saturating_sub(1);
    let thread_start = if baseline_ordinal == 0 || ready.contains(&0) {
        None
    } else {
        Some(service.materialize_baseline(scope, 0).await)
    };
    let baseline = if ready.contains(&baseline_ordinal) {
        None
    } else {
        Some(service.materialize_baseline(scope, baseline_ordinal).await)
    };
    let run_id_value = run["id"].as_str().unwrap();
    let checkpoint = service
        .capture(scope, run_id_value, root["id"].as_str().unwrap(), ordinal, captured_at)
        .await;
    let stamp = at(captured_at);
    let mut events = vec![];
    events.extend(thread_start.map(|checkpoint| ("checkpoint.captured", checkpoint)));
    events.extend(baseline.map(|checkpoint| ("checkpoint.captured", checkpoint)));
    let item = json!({"id":format!("turn-item:provider:{}:native-item:{}",encode_part(provider_thread["driver"].as_str().unwrap_or("")),encode_part(&format!("checkpoint:{}",checkpoint["id"].as_str().unwrap()))),
        "threadId":run["threadId"],"runId":run["id"],"nodeId":root["id"],"providerThreadId":provider_thread["id"],"providerTurnId":root["providerTurnId"],
        "nativeItemRef":null,"parentItemId":null,"ordinal":ordinal*100+99,"status":"completed","title":null,"startedAt":stamp,"completedAt":stamp,"updatedAt":stamp,
        "type":"checkpoint","checkpointId":checkpoint["id"],"scopeId":checkpoint["scopeId"],"files":checkpoint["files"]});
    // delegatedCompletion is left out so a newer cohort write is not overwritten by this snapshot.
    let mut run = run.clone();
    run.as_object_mut().unwrap().remove("delegatedCompletion");
    run["checkpointId"] = checkpoint["id"].clone();
    if !stopped {
        run["status"] = json!("completed");
        run["completedAt"] = json!(stamp);
    }
    events.push(("checkpoint.captured", checkpoint));
    events.push(("turn-item.updated", item));
    events.push(("run.updated", run));
    if !stopped {
        let mut root = root.clone();
        root["status"] = json!("completed");
        root["completedAt"] = json!(stamp);
        root["checkpointScopeId"] = scope["id"].clone();
        events.push(("node.updated", root));
    }
    Ok(Plan::Events(events))
}

/// Run one claimed `checkpoint.capture` effect. Git work happens before the commit, as in the
/// source; the commit re-checks that the run is still the one that was planned.
pub(crate) async fn execute(store: &Store, effect: &Effect) -> Result<(), String> {
    let thread_id = effect.thread_id.as_str();
    let run_id = effect.request["runId"].as_str().ok_or("Capture effect has no run.")?;
    let scope_id = effect.request["scopeId"].as_str().ok_or("Capture effect has no scope.")?;
    let projection = store
        .projection("thread", thread_id)
        .map_err(|error| error.to_string())?
        .ok_or("Thread not found.")?;
    let now = Utc::now();
    let Plan::Events(events) = plan_capture(shared(), &projection, run_id, scope_id, now).await?
    else {
        return Ok(());
    };
    let planned_status = by_id(&projection, "runs", &json!(run_id)).map(|run| run["status"].clone());
    let command_id = format!("command:effect:checkpoint.capture:{run_id}");
    let receipt = store
        .dispatch(
            &command_id,
            "thread",
            thread_id,
            "checkpoint.capture",
            now,
            |transaction| {
                let current = read_projection(transaction, "thread", thread_id)?
                    .ok_or_else(|| StoreError::InvalidCommand("Thread not found.".into()))?;
                let run = by_id(&current, "runs", &json!(run_id));
                if run.map(|run| run["status"].clone()) != planned_status
                    || run.is_some_and(|run| !run["checkpointId"].is_null())
                {
                    return Ok(Decision::Accepted { events: vec![], effects: vec![] });
                }
                let events = events
                    .into_iter()
                    .map(|(kind, payload)| {
                        let payload = t3_contracts::normalize_event_payload(kind, payload)
                            .map_err(|error| StoreError::InvalidCommand(error.to_string()))?;
                        Ok(event(thread_id, &command_id, kind, payload, now))
                    })
                    .collect::<Result<Vec<_>, StoreError>>()?;
                Ok(Decision::Accepted { events, effects: vec![] })
            },
            thread::reduce,
        )
        .map_err(|error| error.to_string())?;
    let _ = receipt;
    refresh_after_capture(store, thread_id, run_id, scope_id).await;
    // The run is no longer blocking: a queued message may start.
    crate::codex_runtime::settle_queue(store, thread_id, None).map_err(|error| error.to_string())
}

/// What the turn left behind is visible to the user: pull requests held for the project are
/// stale, and when the thread works on its own branch, the status of that checkout (and the
/// pull request it shows) is read again. Best effort, like the original's observer.
async fn refresh_after_capture(store: &Store, thread_id: &str, run_id: &str, scope_id: &str) {
    let Ok(Some(projection)) = store.projection("thread", thread_id) else { return };
    if let Some(project_id) = projection["thread"]["projectId"].as_str() {
        crate::pull_request_service::PullRequestService::shared().refresh_after_turn(project_id);
    }
    let Some(cwd) = by_id(&projection, "checkpointScopes", &json!(scope_id)).and_then(|scope| scope["cwd"].as_str()) else { return };
    let status = crate::vcs_service::VcsService::shared().status();
    let Ok(local) = status.refresh_local_status(cwd).await else { return };
    let thread = &projection["thread"];
    if local.ref_name.is_none() || local.is_default_ref || thread["branch"].as_str() != local.ref_name.as_deref() {
        return;
    }
    // Another run is active on the thread: its own completion refreshes.
    if thread["activeRunId"].as_str().is_some_and(|active| active != run_id) {
        return;
    }
    let hooks = crate::vcs_service::VcsContext { store: store.clone(), settings: None, background: None }.hooks();
    if let Err(error) = status.refresh_pull_request_status(cwd, &hooks).await {
        tracing::warn!(thread_id=%thread_id,cwd=%cwd,error=%error,"failed to refresh pull request status after run completion");
    }
}

/// Before a turn starts, snapshot the workspace as the previous ordinal's checkpoint unless one
/// exists. A failure only means the turn runs without a baseline.
pub(crate) async fn capture_baseline(store: &Store, thread_id: &str, run_id: Option<&str>) {
    let Some(run_id) = run_id else { return };
    let Ok(Some(projection)) = store.projection("thread", thread_id) else { return };
    let Some(run) = by_id(&projection, "runs", &json!(run_id)) else { return };
    let Some(root) = by_id(&projection, "nodes", &run["rootNodeId"]) else { return };
    let Some(scope) = by_id(&projection, "checkpointScopes", &root["checkpointScopeId"]) else {
        return;
    };
    let ordinal = run["ordinal"].as_u64().unwrap_or(1).saturating_sub(1);
    if let Err(error) = shared().capture_baseline(scope, ordinal).await {
        tracing::warn!(run_id, %error, "checkpoint baseline capture failed; starting provider without a baseline");
    }
}

/// Outbox schedule for a failed capture: 100ms doubling to 30s, five attempts, as the source.
pub(crate) fn retry_delay(attempt_count: u64) -> Option<chrono::Duration> {
    (attempt_count < 5).then(|| {
        chrono::Duration::milliseconds((100u64 << attempt_count.saturating_sub(1).min(20)).min(30_000) as i64)
    })
}


/// The capture effect of a run whose turn just ended; none when its node has no scope.
/// Source: the finalization effects of `RunExecutionService.writeFinalRunEvents`.
pub(crate) fn capture_effect(run: &Value, root_node: &Value) -> Option<NewEffect> {
    let (run_id, scope_id) = (run["id"].as_str()?, root_node["checkpointScopeId"].as_str()?);
    Some(NewEffect {
        id: format!("effect:checkpoint.capture:{run_id}"),
        command_id: format!("command:effect:checkpoint.capture:{run_id}"),
        thread_id: run["threadId"].as_str()?.into(),
        request: json!({"type":"checkpoint.capture","runId":run_id,"scopeId":scope_id}),
        available_at: at(Utc::now()),
    })
}

/// `codex_runtime::commit` for provider-side transitions that also enqueue effects, so a
/// terminal run and its follow-up work commit in one transaction.
pub(crate) fn commit_with_effects(
    store: &Store,
    thread_id: &str,
    plan: impl FnOnce(&Value) -> Result<(Vec<(&'static str, Value)>, Vec<NewEffect>), StoreError>,
) -> Result<(), StoreError> {
    let command_id = format!("provider-event:{}", uuid::Uuid::new_v4());
    let now = Utc::now();
    store.dispatch(
        &command_id,
        "thread",
        thread_id,
        "provider.event",
        now,
        |transaction| {
            let projection = read_projection(transaction, "thread", thread_id)?
                .ok_or_else(|| StoreError::InvalidCommand("Thread not found.".into()))?;
            let (events, effects) = plan(&projection)?;
            let events = events
                .into_iter()
                .map(|(kind, payload)| {
                    let payload = t3_contracts::normalize_event_payload(kind, payload)
                        .map_err(|error| StoreError::InvalidCommand(error.to_string()))?;
                    Ok(event(thread_id, &command_id, kind, payload, now))
                })
                .collect::<Result<Vec<_>, StoreError>>()?;
            Ok(Decision::Accepted { events, effects })
        },
        thread::reduce,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoint_service::{CheckpointService, ref_for_scope_ordinal, scripted::Script};

    /// Replays the original capture service over a scripted repository: which checkpoints and
    /// events it writes, in which order, and which git calls it makes.
    #[tokio::test]
    async fn capture_decisions_match_the_original() {
        let now: DateTime<Utc> = "2026-03-01T10:05:00.000Z".parse().unwrap();
        let mut count = 0;
        for line in include_str!("../tests/fixtures/checkpoint-orchestration.jsonl").lines() {
            let row: Value = serde_json::from_str(line).unwrap();
            if row["operation"] != "capture" {
                continue;
            }
            let (context, store) = (&row["context"], &row["store"]);
            let scope_id = "checkpoint-scope:thread:thread%3Acapture:name:root";
            let script = Script {
                repository: store["repository"] == true,
                refs: store["existing"].as_array().unwrap().iter().map(|ordinal| ref_for_scope_ordinal(scope_id, ordinal.as_u64().unwrap())).collect::<Vec<_>>().into(),
                fail_capture: store["captureFails"] == true,
                fail_has_ref: store["hasRefFails"] == true,
                fail_diff: store["diffFails"] == true,
                numstat: store["numstat"].as_str().unwrap_or("").into(),
                ..Default::default()
            };
            let service = CheckpointService::new(script);
            let one = |field: &str| context.get(field).filter(|value| !value.is_null()).cloned().into_iter().collect::<Vec<_>>();
            let projection = json!({
                "runs": one("run"), "nodes": one("rootNode"), "checkpointScopes": one("scope"), "providerThreads": one("providerThread"),
                "checkpoints": context["readyCheckpointOrdinals"].as_array().unwrap().iter().map(|ordinal| json!({"scopeId":scope_id,"ordinalWithinScope":ordinal,"status":"ready"})).collect::<Vec<_>>(),
            });
            let label = row["scenario"].to_string();
            let outcome = plan_capture(&service, &projection, row["runId"].as_str().unwrap(), scope_id, now).await;
            let output = &row["output"];
            match outcome {
                Err(cause) => assert_eq!(json!({"error":cause}), *output, "{label}"),
                Ok(Plan::Skip) => assert_eq!(json!({"skip":true}), *output, "{label}"),
                Ok(Plan::Events(events)) => {
                    let stamp = at(now);
                    let mut got = json!(events.iter().map(|(kind, payload)| json!({"type":kind,"payload":payload})).collect::<Vec<_>>());
                    let text = serde_json::to_string(&got).unwrap().replace(&stamp, "NOW");
                    got = serde_json::from_str(&text).unwrap();
                    let want = json!(output["events"].as_array().unwrap().iter().map(|event| json!({"type":event["type"],"payload":event["payload"]})).collect::<Vec<_>>());
                    assert_eq!(got, want, "{label}");
                    assert_eq!(output["effects"], json!([]), "{label}");
                    // Every payload is valid on the wire.
                    for (kind, payload) in events {
                        t3_contracts::normalize_event_payload(kind, payload).unwrap_or_else(|error| panic!("{label} {kind}: {error}"));
                    }
                }
            }
            assert_eq!(json!(*service.git().calls.lock().unwrap()), row["calls"], "{label}");
            count += 1;
        }
        assert_eq!(count, 25);
    }

    /// A server that stopped between a turn ending and its checkpoint capture: startup leaves the
    /// waiting run to the replayed capture effect, which completes it. A waiting run with no
    /// capture pending (an approval the dead process was waiting on) is still cancelled.
    #[tokio::test]
    async fn startup_leaves_a_run_waiting_for_its_capture_and_the_replayed_effect_completes_it() {
        let store = Store::memory().unwrap();
        let thread = |id: &str| json!({"createdBy":"user","creationSource":"web","id":id,"projectId":"project:1","title":"T","modelSelection":{"instanceId":"codex","model":"gpt"},"providerInstanceId":"codex","runtimeMode":"approval-required","interactionMode":"default","branch":null,"worktreePath":null,"activeProviderThreadId":"provider-thread:1","lineage":{"parentThreadId":null,"relationshipToParent":null,"rootThreadId":id},"forkedFrom":null,"createdAt":"2026-04-01T00:00:00.000Z","archivedAt":null,"deletedAt":null,"updatedAt":"2026-04-01T00:00:00.000Z"});
        let waiting = |thread_id: &str| {
            let mut projection = thread::projection_after(None, &event(thread_id, "seed", "thread.created", t3_contracts::normalize_event_payload("thread.created", thread(thread_id)).unwrap(), Utc::now())).unwrap();
            let scope = format!("scope:{thread_id}");
            projection["runs"] = json!([{"id":format!("run:{thread_id}"),"threadId":thread_id,"ordinal":1,"providerInstanceId":"codex","modelSelection":{"instanceId":"codex","model":"gpt"},"providerThreadId":"provider-thread:1","userMessageId":"m","rootNodeId":format!("node:{thread_id}"),"activeAttemptId":format!("attempt:{thread_id}"),"status":"waiting","requestedAt":"2026-04-01T00:00:00.000Z","startedAt":"2026-04-01T00:00:01.000Z","completedAt":null,"checkpointId":null,"contextHandoffId":null}]);
            projection["attempts"] = json!([{"id":format!("attempt:{thread_id}"),"runId":format!("run:{thread_id}"),"attemptOrdinal":1,"rootNodeId":format!("node:{thread_id}"),"providerInstanceId":"codex","providerThreadId":"provider-thread:1","providerTurnId":null,"reason":"initial","status":"running","startedAt":"2026-04-01T00:00:01.000Z","completedAt":null}]);
            projection["nodes"] = json!([{"id":format!("node:{thread_id}"),"threadId":thread_id,"runId":format!("run:{thread_id}"),"parentNodeId":null,"rootNodeId":format!("node:{thread_id}"),"kind":"root_turn","status":"waiting","countsForRun":true,"providerThreadId":"provider-thread:1","providerTurnId":null,"nativeItemRef":null,"runtimeRequestId":null,"checkpointScopeId":scope,"startedAt":"2026-04-01T00:00:01.000Z","completedAt":null}]);
            projection["providerThreads"] = json!([{"id":"provider-thread:1","driver":"codex","providerInstanceId":"codex","providerSessionId":null,"appThreadId":thread_id,"ownerNodeId":null,"nativeThreadRef":null,"nativeConversationHeadRef":null,"status":"idle","firstRunOrdinal":1,"lastRunOrdinal":1,"handoffIds":[],"forkedFrom":null,"createdAt":"2026-04-01T00:00:00.000Z","updatedAt":"2026-04-01T00:00:00.000Z"}]);
            // Not a directory: the capture records a missing checkpoint without running git.
            projection["checkpointScopes"] = json!([{"id":scope,"threadId":thread_id,"runId":format!("run:{thread_id}"),"nodeId":format!("node:{thread_id}"),"parentScopeId":null,"providerThreadId":"provider-thread:1","kind":"root_run","ordinalWithinParent":0,"advancesAppRunCount":true,"cwd":"/nonexistent/checkpoint-recovery","createdAt":"2026-04-01T00:00:00.000Z"}]);
            projection
        };
        store.transaction(|transaction| {
            crate::persistence::write_projection(transaction, "thread", "capturing", &waiting("capturing"))?;
            crate::persistence::write_projection(transaction, "thread", "approving", &waiting("approving"))
        }).unwrap();
        let effect = capture_effect(&waiting("capturing")["runs"][0], &waiting("capturing")["nodes"][0]).unwrap();
        store.dispatch("pending-capture", "thread", "capturing", "provider.event", Utc::now(), |_| Ok(Decision::Accepted { events: vec![], effects: vec![effect.clone()] }), thread::reduce).unwrap();

        let lease = store.acquire_runtime_lease().unwrap();
        crate::codex_runtime::recover(&store, &lease).unwrap();
        let status = |thread_id: &str| store.projection("thread", thread_id).unwrap().unwrap()["runs"][0]["status"].clone();
        assert_eq!((status("capturing"), status("approving")), (json!("waiting"), json!("cancelled")));
        assert_eq!(store.effect(&effect.id).unwrap().unwrap().status, "pending");

        let claimed = store.claim_effect("replay", Utc::now(), chrono::Duration::minutes(1)).unwrap().unwrap();
        assert_eq!(claimed.id, effect.id);
        execute(&store, &claimed).await.unwrap();
        let done = store.projection("thread", "capturing").unwrap().unwrap();
        assert_eq!((done["runs"][0]["status"].clone(), done["nodes"][0]["status"].clone()), (json!("completed"), json!("completed")));
        let recorded = done["checkpoints"].as_array().unwrap().iter().find(|checkpoint| checkpoint["ordinalWithinScope"] == 1).unwrap();
        assert_eq!((recorded["status"].clone(), done["runs"][0]["checkpointId"].clone()), (json!("missing"), recorded["id"].clone()));
        drop(lease);
    }
}
