//! `checkpoint.rollback`: restore a thread to a checkpoint (port of the `checkpoint.rollback`
//! half of `Orchestrator.ts`, `CheckpointRestoreSafety.ts` and `CheckpointRollbackService.ts`).
//!
//! Admission (`plan`) validates the target and queues a `provider-thread.rollback` effect. The
//! effect rewinds the provider conversation when runs follow the target, restores the worktree,
//! deletes the refs of later checkpoints, and marks them `stale` and the later runs
//! `rolled_back`. A rollback that fails every retry is recorded on the thread as `rollbackFailure`.
use crate::{
    checkpoint_service::shared,
    execution::{at, event},
    persistence::{
        Decision, Effect, NewEffect, Store, StoreError, read_projection, read_projections,
    },
    thread,
};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::{collections::HashSet, path::Path};

pub(crate) const SHARED_WORKSPACE_RESTORE_MESSAGE: &str = "File restore requires an isolated worktree. This workspace may contain changes from another thread. Rewind the conversation without restoring files instead.";
pub(crate) const ROLLBACK_FAILED_MESSAGE: &str = "The provider could not roll back this conversation. Try again; if it keeps failing, check the provider and server logs.";
const MAX_ATTEMPTS: u64 = 5;

fn rows<'a>(projection: &'a Value, field: &str) -> &'a [Value] {
    projection[field]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
}
fn by_id<'a>(projection: &'a Value, field: &str, id: &Value) -> Option<&'a Value> {
    rows(projection, field).iter().find(|row| row["id"] == *id)
}
fn rejected(message: impl Into<String>) -> StoreError {
    StoreError::InvalidCommand(message.into())
}

/// The adapter's default capabilities, which `ensureRollback` checks. `None` for a driver the
/// native server does not run.
pub(crate) fn driver_capabilities(driver: &str) -> Option<Value> {
    let encoded = match driver {
        "codex" => include_str!("codex-capabilities.json"),
        "claudeAgent" => include_str!("claude-capabilities.json"),
        "acpRegistry" | "antigravity" => include_str!("acp-capabilities.json"),
        "grok" => include_str!("grok-capabilities.json"),
        _ => return None,
    };
    serde_json::from_str(encoded).ok()
}

/// `CommandPolicy.ensureRollback`.
fn ensure_rollback(command: &Value, instance: &str, capabilities: &Value) -> Result<(), StoreError> {
    let unsupported = |capability: &str, detail: &str| {
        rejected(format!(
            "{instance} cannot satisfy {capability} for command {}: {detail}",
            command["commandId"].as_str().unwrap_or("")
        ))
    };
    if capabilities["threads"]["canRollbackThread"] != true
        || capabilities["checkpointing"]["providerCanRollbackConversation"] != true
    {
        return Err(unsupported("rollback", "providerInstanceId conversation rollback is unavailable"));
    }
    if capabilities["checkpointing"]["providerRollbackReturnsSnapshot"] != true {
        return Err(unsupported(
            "rollback_snapshot",
            "rollback must return a providerInstanceId thread snapshot",
        ));
    }
    Ok(())
}

/// `isCheckpointRestoreIsolated`: a checkpoint snapshots the whole checkout, so restoring one is
/// only safe when no other thread works in the same directory. `others` are all thread
/// projections; `project_root` is a project's workspace root, `None` when the project is gone.
/// `Err` is a filesystem failure other than a missing path.
pub(crate) fn is_restore_isolated(
    thread: &Value,
    scope_cwd: &str,
    others: &[Value],
    project_root: &dyn Fn(&str) -> Option<String>,
    real_path: &dyn Fn(&str) -> std::io::Result<String>,
) -> Result<bool, String> {
    let Some(worktree) = thread["worktreePath"].as_str() else {
        return Ok(false);
    };
    let cwd = real_path(scope_cwd).map_err(|error| error.to_string())?;
    if cwd != real_path(worktree).map_err(|error| error.to_string())? {
        return Ok(false);
    }
    let contains = |parent: &str, child: &str| Path::new(child).starts_with(parent);
    let mut checked = HashSet::new();
    for other in others {
        let other_thread = &other["thread"];
        if other_thread["id"] == thread["id"] || !other_thread["deletedAt"].is_null() {
            continue;
        }
        let mut paths: Vec<String> = vec![];
        paths.extend(other_thread["worktreePath"].as_str().map(str::to_owned));
        paths.extend(
            rows(other, "checkpointScopes")
                .iter()
                .filter_map(|scope| scope["cwd"].as_str().map(str::to_owned)),
        );
        // A session shared across threads keeps the cwd it was opened with, often another
        // thread's; each turn runs in the thread's own worktree or project root, covered here.
        paths.extend(
            rows(other, "providerSessions")
                .iter()
                .filter(|session| {
                    session["status"] != "stopped"
                        && session["capabilities"]["sessions"]["supportsMultipleProviderThreadsPerSession"]
                            != true
                })
                .filter_map(|session| session["cwd"].as_str().map(str::to_owned)),
        );
        if other_thread["worktreePath"].is_null() {
            // A thread without a worktree works in its project's checkout.
            match project_root(other_thread["projectId"].as_str().unwrap_or("")) {
                Some(root) => paths.push(root),
                None => return Ok(false),
            }
        }
        for candidate in paths {
            if !checked.insert(candidate.clone()) {
                continue;
            }
            let other_cwd = match real_path(&candidate) {
                Ok(path) => path,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.to_string()),
            };
            if contains(&cwd, &other_cwd) || contains(&other_cwd, &cwd) {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

fn disk_path(path: &str) -> std::io::Result<String> {
    std::fs::canonicalize(path).map(|path| path.to_string_lossy().into_owned())
}

/// `isCheckpointRestoreIsolated` against the live store and disk.
fn isolated_in(
    connection: &rusqlite::Connection,
    thread: &Value,
    scope_cwd: &str,
) -> Result<bool, String> {
    let others = read_projections(connection, "thread").map_err(|error| error.to_string())?;
    is_restore_isolated(
        thread,
        scope_cwd,
        &others,
        &|project| {
            read_projection(connection, "project", project)
                .ok()
                .flatten()
                .and_then(|project| project["workspaceRoot"].as_str().map(str::to_owned))
        },
        &disk_path,
    )
}

/// `providerTurnForRun`: the run's latest provider turn, else the one its attempt recorded.
pub(crate) fn provider_turn_for_run<'a>(projection: &'a Value, run: &Value) -> Option<&'a Value> {
    let attempt_id = run["activeAttemptId"].as_str()?;
    let latest = latest_turn_for_attempt(projection, attempt_id);
    latest.or_else(|| {
        let attempt = by_id(projection, "attempts", &json!(attempt_id))?;
        rows(projection, "providerTurns")
            .iter()
            .find(|turn| turn["id"] == attempt["providerTurnId"])
    })
}
fn latest_turn_for_attempt<'a>(projection: &'a Value, attempt_id: &str) -> Option<&'a Value> {
    let mut latest: Option<&Value> = None;
    for turn in rows(projection, "providerTurns") {
        if turn["runAttemptId"] != attempt_id {
            continue;
        }
        if latest.is_none_or(|current| turn["ordinal"].as_u64() > current["ordinal"].as_u64()) {
            latest = Some(turn);
        }
    }
    latest
}

/// Admission of `checkpoint.rollback`. `capabilities` are the active provider adapter's.
pub(crate) fn plan(
    command: &Value,
    projection: &Value,
    transaction: &rusqlite::Transaction<'_>,
    now: DateTime<Utc>,
    capabilities: &Value,
) -> Result<Decision, StoreError> {
    plan_with(command, projection, now, capabilities, &|thread, cwd| {
        isolated_in(transaction, thread, cwd)
    })
}

/// `plan`, with the restore-safety check supplied.
pub(crate) fn plan_with(
    command: &Value,
    projection: &Value,
    now: DateTime<Utc>,
    capabilities: &Value,
    isolated: &dyn Fn(&Value, &str) -> Result<bool, String>,
) -> Result<Decision, StoreError> {
    let thread = &projection["thread"];
    let (thread_id, command_id) = (
        command["threadId"].as_str().unwrap(),
        command["commandId"].as_str().unwrap(),
    );
    let provider_thread = by_id(projection, "providerThreads", &thread["activeProviderThreadId"])
        .ok_or_else(|| rejected("No active provider thread exists for rollback."))?;
    if provider_thread["providerSessionId"].is_null() {
        return Err(rejected(format!(
            "Provider thread {} has no provider session.",
            provider_thread["id"].as_str().unwrap_or("")
        )));
    }
    let instance = thread["modelSelection"]["instanceId"].as_str().unwrap_or("");
    ensure_rollback(command, instance, capabilities)?;
    let checkpoint_id = &command["checkpointId"];
    let id_text = checkpoint_id.as_str().unwrap_or("");
    let target = by_id(projection, "checkpoints", checkpoint_id)
        .ok_or_else(|| rejected(format!("Checkpoint {id_text} was not found.")))?;
    if target["status"] != "ready" {
        return Err(rejected(format!(
            "Checkpoint {id_text} is {} and cannot be restored.",
            target["status"].as_str().unwrap_or("")
        )));
    }
    let scope = by_id(projection, "checkpointScopes", &target["scopeId"]).ok_or_else(|| {
        rejected(format!(
            "Checkpoint scope {} was not found.",
            target["scopeId"].as_str().unwrap_or("")
        ))
    })?;
    if scope["id"] != command["scopeId"] {
        return Err(rejected(format!(
            "Checkpoint {id_text} belongs to scope {}, not {}.",
            scope["id"].as_str().unwrap_or(""),
            command["scopeId"].as_str().unwrap_or("")
        )));
    }
    if command["restoreFiles"] != false {
        if !isolated(thread, scope["cwd"].as_str().unwrap_or("")).map_err(rejected)? {
            return Err(rejected(SHARED_WORKSPACE_RESTORE_MESSAGE));
        }
    }
    let target_ordinal = target["appRunOrdinal"].as_u64().unwrap_or(0);
    if target_ordinal > 0 {
        let target_turn = rows(projection, "runs")
            .iter()
            .find(|run| run["ordinal"].as_u64() == Some(target_ordinal))
            .and_then(|run| provider_turn_for_run(projection, run));
        let Some(target_turn) = target_turn else {
            return Err(rejected(format!(
                "Cannot roll back to checkpoint {id_text}: its provider turn is unavailable."
            )));
        };
        if target_turn["providerThreadId"] != provider_thread["id"] {
            return Err(rejected(format!(
                "Cannot roll back provider thread {} to checkpoint {id_text}: target provider turn {} belongs to provider thread {}.",
                provider_thread["id"].as_str().unwrap_or(""),
                target_turn["id"].as_str().unwrap_or(""),
                target_turn["providerThreadId"].as_str().unwrap_or("")
            )));
        }
    }
    let stamp = at(now);
    // This rollback becomes the only one whose failure the thread records.
    let mut updated = thread.clone();
    updated["rollbackRequestId"] = json!(command_id);
    updated["rollbackFailure"] = Value::Null;
    updated["updatedAt"] = json!(stamp);
    let mut request = json!({"type":"provider-thread.rollback","providerThreadId":provider_thread["id"],"checkpointId":target["id"],"scopeId":scope["id"]});
    if let Some(restore) = command.get("restoreFiles") {
        request["restoreFiles"] = restore.clone();
    }
    let build = |kind: &str, payload: Value| {
        let payload = t3_contracts::normalize_event_payload(kind, payload)
            .map_err(|error| StoreError::InvalidCommand(error.to_string()))?;
        Ok::<_, StoreError>(event(thread_id, command_id, kind, payload, now))
    };
    Ok(Decision::Accepted {
        events: vec![
            build("thread.metadata-updated", updated)?,
            build(
                "checkpoint.rollback-requested",
                json!({"scopeId":scope["id"],"checkpointId":target["id"],"requestedAt":stamp}),
            )?,
        ],
        effects: vec![NewEffect {
            id: format!(
                "effect:{command_id}:provider-thread.rollback:{}:{id_text}",
                provider_thread["id"].as_str().unwrap_or("")
            ),
            command_id: command_id.into(),
            thread_id: thread_id.into(),
            request,
            available_at: stamp,
        }],
    })
}

/// Everything the effect decided before touching the provider or the worktree.
pub(crate) struct Prepared {
    pub thread_id: String,
    pub provider_thread: Value,
    pub checkpoint: Value,
    pub scope: Value,
    pub target_ordinal: u64,
    pub restore_files: bool,
    runs: Vec<Value>,
    nodes: Vec<Value>,
    stale: Vec<Value>,
    /// The `ProviderAdapterV2RollbackTarget`.
    pub target: Value,
    /// Provider turns of this provider thread that still exist in the conversation.
    pub provider_thread_turns: Vec<Value>,
}
impl Prepared {
    /// With no run after the target the conversation already ends there.
    pub fn needs_provider(&self) -> bool {
        !self.runs.is_empty()
    }
}

/// The `CheckpointRollbackExecutionError` messages. The client only ever sees
/// `ROLLBACK_FAILED_MESSAGE`; these go to the log.
fn target_invalid(thread: &str, provider_thread: &str, checkpoint: &str) -> String {
    format!("Rollback target {checkpoint} for provider thread {provider_thread} on thread {thread} is incomplete or invalid.")
}

/// The validation half of `CheckpointRollbackService.execute`.
pub(crate) fn prepare(store: &Store, effect: &Effect) -> Result<Prepared, String> {
    let projection = store
        .projection("thread", &effect.thread_id)
        .map_err(|error| error.to_string())?
        .ok_or("Thread not found.")?;
    prepare_with(&projection, &effect.thread_id, &effect.request, &|thread, cwd| {
        store
            .read(|connection| Ok(isolated_in(connection, thread, cwd)))
            .map_err(|error| error.to_string())?
    })
}

/// `prepare` over a thread projection, with the restore-safety check supplied.
pub(crate) fn prepare_with(
    projection: &Value,
    thread_id: &str,
    request: &Value,
    isolated: &dyn Fn(&Value, &str) -> Result<bool, String>,
) -> Result<Prepared, String> {
    let projection = projection.clone();
    let (provider_thread_id, checkpoint_id, scope_id) = (
        request["providerThreadId"].as_str().unwrap_or(""),
        request["checkpointId"].as_str().unwrap_or(""),
        request["scopeId"].as_str().unwrap_or(""),
    );
    let provider_thread = by_id(&projection, "providerThreads", &json!(provider_thread_id));
    let checkpoint = by_id(&projection, "checkpoints", &json!(checkpoint_id));
    let scope = by_id(&projection, "checkpointScopes", &json!(scope_id));
    let (Some(provider_thread), Some(checkpoint), Some(scope)) = (provider_thread, checkpoint, scope)
    else {
        return Err(target_invalid(thread_id, provider_thread_id, checkpoint_id));
    };
    if provider_thread["providerSessionId"].is_null()
        || checkpoint["scopeId"] != scope["id"]
        || checkpoint["status"] != "ready"
    {
        return Err(target_invalid(thread_id, provider_thread_id, checkpoint_id));
    }
    let thread = &projection["thread"];
    if provider_thread["id"] != thread["activeProviderThreadId"]
        || provider_thread["providerInstanceId"] != thread["modelSelection"]["instanceId"]
    {
        return Err(format!(
            "Active provider changed before rollback target {checkpoint_id} could execute on thread {thread_id}."
        ));
    }
    let restore_files = request["restoreFiles"] != false;
    if restore_files {
        if !isolated(thread, scope["cwd"].as_str().unwrap_or(""))? {
            return Err(SHARED_WORKSPACE_RESTORE_MESSAGE.into());
        }
    }
    let target_ordinal = checkpoint["appRunOrdinal"].as_u64().unwrap_or(0);
    // Stopped and failed runs after the target leave the provider conversation too, so they
    // must not stay visible.
    let runs: Vec<Value> = rows(&projection, "runs")
        .iter()
        .filter(|run| {
            run["ordinal"].as_u64().unwrap_or(0) > target_ordinal
                && matches!(
                    run["status"].as_str(),
                    Some("completed" | "interrupted" | "failed" | "cancelled")
                )
        })
        .cloned()
        .collect();
    // Rolled-back turns stay in the audit history but no longer exist in the provider
    // conversation, and must not be counted by a later rewind.
    let rolled_back_attempts: HashSet<&str> = rows(&projection, "attempts")
        .iter()
        .filter(|attempt| {
            rows(&projection, "runs")
                .iter()
                .any(|run| run["status"] == "rolled_back" && run["id"] == attempt["runId"])
        })
        .filter_map(|attempt| attempt["id"].as_str())
        .collect();
    let provider_thread_turns: Vec<Value> = rows(&projection, "providerTurns")
        .iter()
        .filter(|turn| {
            turn["providerThreadId"] == provider_thread["id"]
                && turn["runAttemptId"]
                    .as_str()
                    .is_none_or(|attempt| !rolled_back_attempts.contains(attempt))
        })
        .cloned()
        .collect();
    let target = if target_ordinal == 0 {
        json!({"type":"thread_start","checkpointId":checkpoint["id"],"appRunOrdinal":0})
    } else {
        let turn = rows(&projection, "runs")
            .iter()
            .find(|run| run["ordinal"].as_u64() == Some(target_ordinal))
            .and_then(|run| provider_turn_for_run(&projection, run))
            .filter(|turn| turn["providerThreadId"] == provider_thread["id"])
            .ok_or_else(|| {
                format!("Provider turn for rollback target {checkpoint_id} is unavailable on provider thread {provider_thread_id}.")
            })?;
        json!({"type":"provider_turn","checkpointId":checkpoint["id"],"appRunOrdinal":target_ordinal,"providerTurn":turn})
    };
    let stale: Vec<Value> = rows(&projection, "checkpoints")
        .iter()
        .filter(|candidate| {
            candidate["scopeId"] == scope["id"]
                && candidate["appRunOrdinal"].as_u64().is_some_and(|ordinal| ordinal > target_ordinal)
                && candidate["status"] == "ready"
        })
        .cloned()
        .collect();
    let nodes = runs
        .iter()
        .filter_map(|run| by_id(&projection, "nodes", &run["rootNodeId"]).cloned())
        .collect();
    Ok(Prepared {
        thread_id: thread_id.into(),
        provider_thread: provider_thread.clone(),
        checkpoint: checkpoint.clone(),
        scope: scope.clone(),
        target_ordinal,
        restore_files,
        runs,
        nodes,
        stale,
        target,
        provider_thread_turns,
    })
}

/// The events of one rollback, given the provider thread as the provider left it.
pub(crate) fn rollback_events(
    prepared: &Prepared,
    provider_thread: &Value,
    now: DateTime<Utc>,
) -> Vec<(&'static str, Value)> {
    let stamp = at(now);
    let mut thread = provider_thread.clone();
    thread["lastRunOrdinal"] = if prepared.target_ordinal == 0 {
        Value::Null
    } else {
        json!(prepared.target_ordinal)
    };
    thread["updatedAt"] = json!(stamp);
    let mut events = vec![("provider-thread.updated", thread)];
    for checkpoint in &prepared.stale {
        let mut stale = checkpoint.clone();
        stale["status"] = json!("stale");
        events.push(("checkpoint.captured", stale));
    }
    for run in &prepared.runs {
        let mut rolled_back = run.clone();
        rolled_back["status"] = json!("rolled_back");
        rolled_back["completedAt"] = json!(stamp);
        events.push(("run.updated", rolled_back));
        if let Some(root) = prepared.nodes.iter().find(|node| node["id"] == run["rootNodeId"]) {
            let mut root = root.clone();
            root["status"] = json!("rolled_back");
            root["completedAt"] = json!(stamp);
            events.push(("node.updated", root));
        }
    }
    events
}

/// The rest of `CheckpointRollbackService.execute`: restore the worktree, delete the refs of
/// later checkpoints, and commit the result. `provider_thread` is the provider thread after
/// the provider rewound its conversation (or unchanged when there was nothing to rewind).
pub(crate) async fn finish(
    store: &Store,
    effect: &Effect,
    prepared: Prepared,
    provider_thread: Value,
) -> Result<(), String> {
    finish_with(shared(), store, effect, prepared, provider_thread).await
}

/// `finish` over any checkpoint service.
pub(crate) async fn finish_with<G: crate::checkpoint_service::CheckpointGit>(
    service: &crate::checkpoint_service::CheckpointService<G>,
    store: &Store,
    effect: &Effect,
    prepared: Prepared,
    provider_thread: Value,
) -> Result<(), String> {
    if prepared.restore_files {
        service
            .restore(&prepared.scope, &prepared.checkpoint)
            .await
            .map_err(|error| error.to_string())?;
    }
    if !prepared.stale.is_empty() {
        let refs: Vec<String> = prepared
            .stale
            .iter()
            .filter_map(|checkpoint| checkpoint["ref"].as_str().map(str::to_owned))
            .collect();
        service
            .delete_refs(prepared.scope["cwd"].as_str().unwrap_or(""), &refs)
            .await
            .map_err(|error| error.to_string())?;
    }
    let now = Utc::now();
    let events = rollback_events(&prepared, &provider_thread, now);
    // Replaying an effect whose events already committed finds this receipt and stops.
    let command_id = format!("command:effect:{}:applied", effect.id);
    let thread_id = prepared.thread_id.as_str();
    store
        .dispatch(
            &command_id,
            "thread",
            thread_id,
            "checkpoint.rollback.apply",
            now,
            |_| {
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
    // The conversation now ends at the target: a queued message may run.
    crate::codex_runtime::settle_queue(store, thread_id, None).map_err(|error| error.to_string())
}

/// A rollback needing no provider call (the target is the latest checkpoint, or the caller
/// asked to restore files only).
pub(crate) async fn run_without_provider(store: &Store, effect: &Effect) -> Result<(), String> {
    let prepared = prepare(store, effect)?;
    let provider_thread = prepared.provider_thread.clone();
    finish(store, effect, prepared, provider_thread).await
}

/// Whether `prepare` finds a provider rewind to do. A target that fails validation is run
/// through the no-provider path so the failure is reported there.
pub(crate) fn needs_provider(store: &Store, effect: &Effect) -> bool {
    prepare(store, effect).is_ok_and(|prepared| prepared.needs_provider())
}

/// After a failed attempt: when to retry (100ms doubling to 30s, five attempts), or `None`
/// after recording on the thread that this rollback failed for good.
pub(crate) fn settle_failure(store: &Store, effect: &Effect, error: &str) -> Option<DateTime<Utc>> {
    tracing::warn!(effect = %effect.id, attempt = effect.attempt_count, %error, "Checkpoint rollback failed");
    if effect.attempt_count < MAX_ATTEMPTS {
        let delay = (100u64 << effect.attempt_count.saturating_sub(1).min(20)).min(30_000);
        return Some(Utc::now() + chrono::Duration::milliseconds(delay as i64));
    }
    if let Err(recording) = record_failure(store, effect) {
        tracing::warn!(%recording, "Failed to record rollback failure");
    }
    None
}

/// `checkpoint.rollback.fail`: record that rollback `requestId` failed for good. A newer
/// rollback clears it, and a late failure from a rollback it superseded is rejected.
fn record_failure(store: &Store, effect: &Effect) -> Result<(), StoreError> {
    let thread_id = effect.thread_id.as_str();
    let command_id = format!("{}:rollback-failed", effect.command_id);
    let now = Utc::now();
    store.dispatch(
        &command_id,
        "thread",
        thread_id,
        "checkpoint.rollback.fail",
        now,
        |transaction| {
            let projection = read_projection(transaction, "thread", thread_id)?
                .ok_or_else(|| rejected("Thread not found."))?;
            let thread = &projection["thread"];
            if !thread["deletedAt"].is_null() {
                return Ok(Decision::Accepted { events: vec![], effects: vec![] });
            }
            if thread["rollbackRequestId"] != effect.command_id {
                return Err(rejected(format!(
                    "Rollback {} is no longer the thread's current rollback.",
                    effect.command_id
                )));
            }
            let mut updated = thread.clone();
            updated["rollbackFailure"] = json!({"requestId":effect.command_id,"message":ROLLBACK_FAILED_MESSAGE});
            updated["updatedAt"] = json!(at(now));
            let payload = t3_contracts::normalize_event_payload("thread.metadata-updated", updated)
                .map_err(|error| StoreError::InvalidCommand(error.to_string()))?;
            Ok(Decision::Accepted {
                events: vec![event(thread_id, &command_id, "thread.metadata-updated", payload, now)],
                effects: vec![],
            })
        },
        thread::reduce,
    )?;
    Ok(())
}

// ---- provider side -----------------------------------------------------------------------

/// The provider requests Codex rollback makes. Implemented by the live process and, in tests,
/// by a script.
pub(crate) trait CodexRequests {
    fn request(
        &self,
        method: &str,
        params: Value,
    ) -> impl std::future::Future<Output = Result<Value, String>> + Send;
}
impl CodexRequests for crate::provider_process::ProviderProcess {
    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        crate::provider_process::ProviderProcess::request(
            self,
            method,
            params,
            std::time::Duration::from_secs(30),
        )
        .await
        .map_err(|error| error.to_string())
    }
}

fn is_terminal_turn(turn: &Value) -> bool {
    !turn["nativeTurnRef"].is_null()
        && matches!(
            turn["status"].as_str(),
            Some("completed" | "interrupted" | "failed" | "cancelled")
        )
}

/// `resolveCodexRollbackTurnCount`: how many native turns Codex must drop to end at the target.
pub(crate) fn codex_turn_count(prepared: &Prepared) -> Result<usize, String> {
    let turns = &prepared.provider_thread_turns;
    match prepared.target["type"].as_str() {
        Some("thread_start") => Ok(turns.iter().filter(|turn| is_terminal_turn(turn)).count()),
        _ => {
            let boundary = &prepared.target["providerTurn"];
            let thread_id = prepared.provider_thread["id"].as_str().unwrap_or("");
            let boundary_id = boundary["id"].as_str().unwrap_or("");
            if boundary["providerThreadId"] != prepared.provider_thread["id"] {
                return Err(format!(
                    "Cannot roll back Codex thread {thread_id} to provider turn {boundary_id}: target turn belongs to provider thread {}.",
                    boundary["providerThreadId"].as_str().unwrap_or("")
                ));
            }
            let Some(found) = turns.iter().find(|turn| turn["id"] == boundary["id"]) else {
                return Err(format!(
                    "Cannot roll back Codex thread {thread_id} to provider turn {boundary_id}: target turn was not found in durable provider turn history."
                ));
            };
            Ok(turns
                .iter()
                .filter(|turn| turn["ordinal"].as_u64() > found["ordinal"].as_u64() && is_terminal_turn(turn))
                .count())
        }
    }
}

/// `revertCodexThread`: Codex 0.156 reverts to a boundary in paginated history, so count back
/// `turns` turns from the newest and revert to the oldest of them.
pub(crate) async fn codex_revert<R: CodexRequests>(
    requests: &R,
    native_thread_id: &str,
    turns: usize,
) -> Result<Value, String> {
    let mut remaining = turns;
    let mut before: Option<String> = None;
    let mut cursor: Option<String> = None;
    let mut visited: HashSet<Option<String>> = HashSet::new();
    while remaining > 0 {
        if !visited.insert(cursor.clone()) {
            return Err("Thread history pagination repeated a cursor.".into());
        }
        let page = requests
            .request(
                "thread/turns/list",
                json!({"threadId":native_thread_id,"cursor":cursor,"limit":remaining.min(100),"sortDirection":"desc","itemsView":"summary"}),
            )
            .await?;
        for turn in page["data"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
            before = turn["id"].as_str().map(str::to_owned);
            remaining -= 1;
            if remaining == 0 {
                break;
            }
        }
        cursor = page["nextCursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    match before {
        None => requests.request("thread/read", json!({"threadId":native_thread_id,"includeTurns":false})).await,
        Some(boundary) => requests.request("thread/revert", json!({"threadId":native_thread_id,"beforeTurnId":boundary})).await,
    }
}

/// `CodexAdapterV2.rollbackThread`: rewind the native conversation and return the provider
/// thread as Codex left it. `resume_params` are the thread runtime parameters a resume sends.
pub(crate) async fn codex_rewind<R: CodexRequests>(
    requests: &R,
    prepared: &Prepared,
    resume_params: Value,
) -> Result<Value, String> {
    let thread = &prepared.provider_thread;
    let native_id = thread["nativeThreadRef"]["nativeId"]
        .as_str()
        .ok_or_else(|| format!("Provider thread {} has no native Codex thread id.", thread["id"].as_str().unwrap_or("")))?;
    let turns = codex_turn_count(prepared)?;
    let head = if prepared.target["type"] == "provider_turn" {
        prepared.target["providerTurn"]["nativeTurnRef"].clone()
    } else {
        Value::Null
    };
    let mut rewound = thread.clone();
    rewound["nativeConversationHeadRef"] = head;
    rewound["status"] = json!("idle");
    if turns == 0 {
        return Ok(rewound);
    }
    // Codex 0.156 can revert paginated history at a turn boundary. Legacy history has no
    // rollback endpoint.
    let metadata = requests
        .request("thread/read", json!({"threadId":native_id,"includeTurns":false}))
        .await?;
    if metadata["thread"]["historyMode"] != "paginated" {
        return Err(format!(
            "Cannot roll back Codex thread {native_id}: the thread uses legacy history, which Codex 0.156 cannot revert."
        ));
    }
    // `thread/revert` only acts on a thread loaded in this app-server process.
    if metadata["thread"]["status"]["type"] == "notLoaded" {
        let mut params = resume_params;
        params["threadId"] = json!(native_id);
        params["excludeTurns"] = json!(true);
        requests.request("thread/resume", params).await?;
    }
    let response = codex_revert(requests, native_id, turns).await?;
    rewound["nativeThreadRef"] = json!({"driver":"codex","nativeId":response["thread"]["id"],"strength":"strong"});
    rewound["updatedAt"] = response["thread"]["updatedAt"]
        .as_i64()
        .and_then(|seconds| DateTime::from_timestamp(seconds, 0))
        .map(|time| json!(at(time)))
        .unwrap_or_else(|| json!(at(Utc::now())));
    Ok(rewound)
}

/// `AcpAdapterV2.rollbackThread`: ACP has no conversation truncation, so the thread gets a
/// fresh native session. The next turn's `session/new` replaces this empty binding; nothing of
/// the rolled-back conversation is loaded.
pub(crate) fn acp_fresh_session(prepared: &Prepared) -> Value {
    let mut thread = prepared.provider_thread.clone();
    thread["nativeThreadRef"] = Value::Null;
    thread["nativeConversationHeadRef"] = Value::Null;
    let mut metadata = thread["nativeMetadata"].as_object().cloned().unwrap_or_default();
    metadata.insert("itemIdentityVersion".into(), json!(2));
    thread["nativeMetadata"] = Value::Object(metadata);
    thread["status"] = json!("idle");
    thread
}

/// Run an ACP rollback in its actor: commit the reset binding and the rollback. The caller then
/// closes the actor, so the next effect connects a new session.
pub(crate) async fn run_acp(store: &Store, effect: &Effect) -> Result<bool, String> {
    let prepared = prepare(store, effect)?;
    let reset = prepared.needs_provider();
    let provider_thread = if reset { acp_fresh_session(&prepared) } else { prepared.provider_thread.clone() };
    finish(store, effect, prepared, provider_thread).await?;
    Ok(reset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn fixtures(operation: &str) -> Vec<Value> {
        include_str!("../tests/fixtures/checkpoint-rollback.jsonl")
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|row| row["operation"] == operation)
            .collect()
    }
    fn stamped(events: &[(&str, Value)], stamp: &str) -> Value {
        let text = serde_json::to_string(&events.iter().map(|(kind, payload)| json!({"type":kind,"payload":payload})).collect::<Vec<_>>()).unwrap();
        serde_json::from_str(&text.replace(stamp, "NOW")).unwrap()
    }
    fn normalized(events: &Value) -> Value {
        json!(events.as_array().unwrap().iter().map(|event| json!({"type":event["type"],"payload":event["payload"]})).collect::<Vec<_>>())
    }

    /// The `checkpoint.rollback` admission decisions and the events and effect they queue.
    #[test]
    fn admission_matches_the_original() {
        let now: DateTime<Utc> = "2026-05-05T05:05:05.005Z".parse().unwrap();
        let rows = fixtures("admission");
        for row in &rows {
            let isolated = |_: &Value, _: &str| match &row["isolated"] {
                Value::Bool(value) => Ok(*value),
                _ => panic!("isolation must not be consulted: {}", row["name"]),
            };
            let outcome = plan_with(&row["command"], &row["projection"], now, &row["capabilities"], &isolated);
            let label = row["name"].to_string();
            match outcome {
                Err(error) => {
                    let StoreError::InvalidCommand(message) = error else { panic!("{label}: {error}") };
                    assert_eq!(json!({"error":message}), row["output"], "{label}");
                }
                Ok(Decision::Accepted { events, effects }) => {
                    let events: Vec<_> = events.iter().map(|event| (event.event_type.as_str(), event.payload.clone())).collect();
                    // The original's payload goes through the same wire normalization (defaults).
                    let expected: Vec<_> = row["output"]["events"].as_array().unwrap().iter().map(|event| (event["type"].as_str().unwrap(), t3_contracts::normalize_event_payload(event["type"].as_str().unwrap(), serde_json::from_str(&event["payload"].to_string().replace("NOW", &at(now))).unwrap()).unwrap())).collect();
                    assert_eq!(stamped(&events, &at(now)), stamped(&expected, &at(now)), "{label}");
                    let effects: Vec<_> = effects.iter().map(|effect| json!({"id":effect.id,"commandId":effect.command_id,"threadId":effect.thread_id,"request":effect.request})).collect();
                    assert_eq!(json!(effects), row["output"]["effects"], "{label}");
                }
                Ok(_) => panic!("{label}: unexpected decision"),
            }
        }
        assert_eq!(rows.len(), 24);
    }

    #[test]
    fn restore_safety_matches_the_original() {
        let rows = fixtures("isolation");
        for row in &rows {
            let neighbours: Vec<Value> = row["neighbours"].as_array().unwrap().iter().cloned().collect();
            let string_set = |field: &str| -> Vec<String> { row[field].as_array().unwrap().iter().map(|value| value.as_str().unwrap().to_owned()).collect() };
            let (missing, denied) = (string_set("missing"), string_set("denied"));
            let real_path = |path: &str| -> std::io::Result<String> {
                if missing.iter().any(|candidate| candidate == path) {
                    return Err(std::io::ErrorKind::NotFound.into());
                }
                if denied.iter().any(|candidate| candidate == path) {
                    return Err(std::io::ErrorKind::PermissionDenied.into());
                }
                Ok(row["realPaths"][path].as_str().unwrap_or(path).to_owned())
            };
            let project_root = |project: &str| row["roots"][project].as_str().map(str::to_owned);
            let result = is_restore_isolated(&row["thread"], row["scopeCwd"].as_str().unwrap(), &neighbours, &project_root, &real_path);
            let got = match result {
                Ok(isolated) => json!({"isolated":isolated}),
                Err(_) => json!({"error":true}),
            };
            assert_eq!(got, row["output"], "{}", row["name"]);
        }
        assert_eq!(rows.len(), 22);
    }

    /// What the effect decides before and after the provider call, for every scenario.
    #[test]
    fn executor_decisions_and_events_match_the_original() {
        let now: DateTime<Utc> = "2026-05-05T05:05:05.005Z".parse().unwrap();
        let mut compared = 0;
        for row in fixtures("execute") {
            if row["behaviour"]["restored"] == false {
                continue;
            }
            let label = row["name"].to_string();
            let isolated = |thread: &Value, cwd: &str| is_restore_isolated(thread, cwd, &[], &|_| None, &|path| Ok(path.to_owned()));
            let prepared = prepare_with(&row["projection"], "thread:1", &row["request"], &isolated);
            let output = &row["output"];
            let prepared = match prepared {
                Err(message) => {
                    assert_eq!(message, output["error"]["message"].as_str().unwrap_or_else(|| panic!("{label}: unexpected failure {message}")), "{label}");
                    continue;
                }
                Ok(prepared) => prepared,
            };
            let mut calls = vec![];
            let mut provider_thread = prepared.provider_thread.clone();
            if prepared.needs_provider() {
                calls.push(json!({"rollback":{"target":prepared.target,"turns":prepared.provider_thread_turns.iter().map(|turn| turn["id"].clone()).collect::<Vec<_>>()}}));
                provider_thread["nativeConversationHeadRef"] = json!({"driver":"codex","nativeId":"rewound","strength":"strong"});
            }
            if prepared.restore_files {
                calls.push(json!({"restore":prepared.checkpoint["ref"]}));
            }
            if !prepared.stale.is_empty() {
                calls.push(json!({"delete":prepared.stale.iter().map(|checkpoint| checkpoint["ref"].clone()).collect::<Vec<_>>()}));
            }
            assert_eq!(json!(calls), output["calls"], "{label}");
            let events = rollback_events(&prepared, &provider_thread, now);
            assert_eq!(stamped(&events, &at(now)), normalized(&output["events"]), "{label}");
            for (kind, payload) in events {
                t3_contracts::normalize_event_payload(kind, payload).unwrap_or_else(|error| panic!("{label} {kind}: {error}"));
            }
            compared += 1;
        }
        assert_eq!(compared, 9);
    }

    #[test]
    fn codex_rollback_turn_counts_match_the_original() {
        let rows = fixtures("codexTurnCount");
        for row in &rows {
            let prepared = Prepared {
                thread_id: "thread:1".into(),
                provider_thread: row["providerThread"].clone(),
                checkpoint: Value::Null,
                scope: Value::Null,
                target_ordinal: 0,
                restore_files: true,
                runs: vec![],
                nodes: vec![],
                stale: vec![],
                target: row["target"].clone(),
                provider_thread_turns: row["providerThreadTurns"].as_array().unwrap().clone(),
            };
            let got = match codex_turn_count(&prepared) {
                Ok(count) => json!({"count":count}),
                Err(message) => json!({"error":message}),
            };
            assert_eq!(got, row["output"], "{}", row["name"]);
        }
        assert_eq!(rows.len(), 8);
    }

    /// Answers Codex's paginated history as the scenario scripts it and records every request.
    struct Script {
        pages: Mutex<Vec<Value>>,
        repeat: bool,
        calls: Mutex<Vec<Value>>,
    }
    impl CodexRequests for Script {
        async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
            self.calls.lock().unwrap().push(json!({"method":method,"params":params}));
            Ok(match method {
                "thread/turns/list" if self.repeat => json!({"data":[{"id":"t5"}],"nextCursor":"same"}),
                "thread/turns/list" => self.pages.lock().unwrap().remove(0),
                _ => json!({"thread":{"id":"native-1"}}),
            })
        }
    }

    #[tokio::test]
    async fn codex_revert_pages_back_to_a_boundary_like_the_original() {
        let rows = fixtures("codexRevert");
        for row in &rows {
            let script = Script { pages: Mutex::new(row["pages"].as_array().unwrap().clone()), repeat: row["mode"] == "repeat", calls: Mutex::new(vec![]) };
            let result = codex_revert(&script, "native-1", row["turns"].as_u64().unwrap() as usize).await;
            let got = match result {
                Ok(_) => json!({"ok":true}),
                Err(message) => json!({"error":message}),
            };
            assert_eq!(got, row["output"], "{}", row["name"]);
            assert_eq!(json!(*script.calls.lock().unwrap()), row["calls"], "{}", row["name"]);
        }
        assert_eq!(rows.len(), 7);
    }

    /// The Codex-side sequence around the revert: nothing to drop, legacy history, and a thread
    /// this process has not loaded.
    #[tokio::test]
    async fn codex_rewind_reads_history_mode_and_resumes_an_unloaded_thread() {
        struct Fake(Mutex<Vec<String>>, &'static str, &'static str);
        impl CodexRequests for Fake {
            async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
                self.0.lock().unwrap().push(format!("{method} {}", params.get("excludeTurns").map_or(String::new(), |value| value.to_string())));
                Ok(match method {
                    "thread/read" => json!({"thread":{"id":"native-1","historyMode":self.1,"status":{"type":self.2}}}),
                    "thread/turns/list" => json!({"data":[{"id":"t2"}],"nextCursor":null}),
                    _ => json!({"thread":{"id":"native-2","updatedAt":1800000500}}),
                })
            }
        }
        let thread = json!({"id":"provider-thread:1","nativeThreadRef":{"driver":"codex","nativeId":"native-1","strength":"strong"},"nativeConversationHeadRef":null,"status":"active"});
        let turn = |ordinal: u64| json!({"id":format!("turn:{ordinal}"),"providerThreadId":"provider-thread:1","ordinal":ordinal,"status":"completed","nativeTurnRef":{"driver":"codex","nativeId":format!("native-turn-{ordinal}"),"strength":"strong"}});
        let prepared = |turns: Vec<Value>, target: Value| Prepared {
            thread_id: "thread:1".into(), provider_thread: thread.clone(), checkpoint: Value::Null, scope: Value::Null, target_ordinal: 1, restore_files: true,
            runs: vec![json!({})], nodes: vec![], stale: vec![], target, provider_thread_turns: turns,
        };
        let target = json!({"type":"provider_turn","providerTurn":turn(1)});

        let nothing = Fake(Mutex::new(vec![]), "paginated", "idle");
        let unchanged = codex_rewind(&nothing, &prepared(vec![turn(1)], target.clone()), json!({})).await.unwrap();
        assert!(nothing.0.lock().unwrap().is_empty(), "no turn follows the target: no request is made");
        assert_eq!((unchanged["status"].clone(), unchanged["nativeConversationHeadRef"].clone()), (json!("idle"), turn(1)["nativeTurnRef"].clone()));

        let legacy = Fake(Mutex::new(vec![]), "legacy", "idle");
        let refused = codex_rewind(&legacy, &prepared(vec![turn(1), turn(2)], target.clone()), json!({})).await.unwrap_err();
        assert!(refused.contains("uses legacy history, which Codex 0.156 cannot revert."), "{refused}");

        let unloaded = Fake(Mutex::new(vec![]), "paginated", "notLoaded");
        let rewound = codex_rewind(&unloaded, &prepared(vec![turn(1), turn(2)], target.clone()), json!({"cwd":"/work"})).await.unwrap();
        assert_eq!(*unloaded.0.lock().unwrap(), ["thread/read ", "thread/resume true", "thread/turns/list ", "thread/revert "]);
        assert_eq!((rewound["nativeThreadRef"]["nativeId"].clone(), rewound["status"].clone(), rewound["updatedAt"].clone()), (json!("native-2"), json!("idle"), json!("2027-01-15T08:08:20.000Z")));

        let loaded = Fake(Mutex::new(vec![]), "paginated", "idle");
        codex_rewind(&loaded, &prepared(vec![turn(1), turn(2)], target), json!({})).await.unwrap();
        assert_eq!(*loaded.0.lock().unwrap(), ["thread/read ", "thread/turns/list ", "thread/revert "]);
    }

    #[test]
    fn acp_rollback_resets_the_native_binding() {
        let prepared = Prepared {
            thread_id: "thread:1".into(),
            provider_thread: json!({"id":"p","nativeThreadRef":{"driver":"acpRegistry","nativeId":"s","strength":"strong"},"nativeConversationHeadRef":{"x":1},"nativeMetadata":{"itemIdentityVersion":1,"other":true},"status":"active"}),
            checkpoint: Value::Null, scope: Value::Null, target_ordinal: 1, restore_files: true, runs: vec![], nodes: vec![], stale: vec![], target: Value::Null, provider_thread_turns: vec![],
        };
        let fresh = acp_fresh_session(&prepared);
        assert_eq!((fresh["nativeThreadRef"].clone(), fresh["nativeConversationHeadRef"].clone(), fresh["status"].clone()), (Value::Null, Value::Null, json!("idle")));
        assert_eq!(fresh["nativeMetadata"], json!({"itemIdentityVersion":2,"other":true}));
    }

    fn effect_for(request: Value, attempt_count: u64) -> Effect {
        Effect {
            id: "effect:command:rb:provider-thread.rollback:provider-thread:1:checkpoint:1".into(),
            command_id: "command:rb".into(),
            thread_id: "thread:1".into(),
            request,
            status: "running".into(),
            attempt_count,
            available_at: "2026-04-01T00:00:00.000Z".into(),
            lease_owner: Some("test".into()),
            lease_expires_at: None,
            created_at: "2026-04-01T00:00:00.000Z".into(),
            updated_at: "2026-04-01T00:00:00.000Z".into(),
            last_error: None,
        }
    }
    fn seeded(name: &str, thread_overrides: Value) -> (Store, Value) {
        let row = fixtures("execute").into_iter().find(|row| row["name"] == name).unwrap();
        let store = Store::memory().unwrap();
        let mut projection = row["projection"].clone();
        for (key, value) in thread_overrides.as_object().unwrap() {
            projection["thread"][key] = value.clone();
        }
        let created = crate::thread::projection_after(
            None,
            &event("thread:1", "seed", "thread.created", t3_contracts::normalize_event_payload("thread.created", projection["thread"].clone()).unwrap(), Utc::now()),
        )
        .unwrap();
        let mut seeded = created;
        for field in ["providerThreads", "checkpoints", "checkpointScopes", "runs", "attempts", "nodes", "providerTurns", "providerSessions"] {
            seeded[field] = projection[field].clone();
        }
        store.transaction(|transaction| crate::persistence::write_projection(transaction, "thread", "thread:1", &seeded)).unwrap();
        (store, row["request"].clone())
    }

    /// The executor over a scripted repository: the worktree is restored, later refs are
    /// deleted, the projection records the rollback, and a replay changes nothing.
    #[tokio::test]
    async fn the_executor_restores_deletes_refs_and_commits_once() {
        use crate::checkpoint_service::{CheckpointService, scripted::Script};
        let (store, request) = seeded("rewind-one-turn", json!({"deletedAt": null}));
        let effect = effect_for(request, 1);
        let script = Script { repository: true, refs: vec!["refs/test/0".to_owned(), "refs/test/1".to_owned(), "refs/test/2".to_owned()].into(), ..Default::default() };
        let service = CheckpointService::new(script);
        let isolated = |thread: &Value, cwd: &str| is_restore_isolated(thread, cwd, &[], &|_| None, &|path| Ok(path.to_owned()));
        let projection = store.projection("thread", "thread:1").unwrap().unwrap();
        let prepared = prepare_with(&projection, "thread:1", &effect.request, &isolated).unwrap();
        let rewound = prepared.provider_thread.clone();
        finish_with(&service, &store, &effect, prepared, rewound.clone()).await.unwrap();
        assert_eq!(*service.git().calls.lock().unwrap(), ["restore refs/test/1", "delete refs/test/2"]);
        let after = store.projection("thread", "thread:1").unwrap().unwrap();
        let status = |field: &str, id: &str| after[field].as_array().unwrap().iter().find(|row| row["id"] == id).unwrap()["status"].clone();
        assert_eq!((status("checkpoints", "checkpoint:1"), status("checkpoints", "checkpoint:2")), (json!("ready"), json!("stale")));
        assert_eq!((status("runs", "run:1"), status("runs", "run:2"), status("nodes", "node:2")), (json!("completed"), json!("rolled_back"), json!("rolled_back")));
        assert_eq!(after["providerThreads"][0]["lastRunOrdinal"], 1);
        let sequence = store.latest_sequence().unwrap();
        // The same effect again (a lease lost after the commit) finds its receipt and stops.
        let prepared = prepare_with(&after, "thread:1", &effect.request, &isolated).unwrap();
        finish_with(&service, &store, &effect, prepared, rewound).await.unwrap();
        assert_eq!(store.latest_sequence().unwrap(), sequence);
    }

    #[test]
    fn a_failure_is_retried_five_times_then_recorded_for_the_current_rollback_only() {
        let (store, request) = seeded("rewind-one-turn", json!({"rollbackRequestId": "command:rb"}));
        let attempt = |count: u64| effect_for(request.clone(), count);
        for (count, millis) in [(1, 100), (2, 200), (3, 400), (4, 800)] {
            let retry = settle_failure(&store, &attempt(count), "boom").expect("retries remain");
            let wait = (retry - Utc::now()).num_milliseconds();
            assert!(wait > millis - 150 && wait <= millis, "attempt {count}: {wait}ms");
        }
        assert!(store.projection("thread", "thread:1").unwrap().unwrap()["thread"]["rollbackFailure"].is_null());
        assert!(settle_failure(&store, &attempt(5), "boom").is_none());
        let thread = store.projection("thread", "thread:1").unwrap().unwrap()["thread"].clone();
        assert_eq!(thread["rollbackFailure"], json!({"requestId":"command:rb","message":ROLLBACK_FAILED_MESSAGE}));
        // A rollback another request has since replaced records nothing.
        let (store, request) = seeded("rewind-one-turn", json!({"rollbackRequestId": "command:newer"}));
        assert!(settle_failure(&store, &effect_for(request, 5), "boom").is_none());
        assert!(store.projection("thread", "thread:1").unwrap().unwrap()["thread"]["rollbackFailure"].is_null());
    }
}
