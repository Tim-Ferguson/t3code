//! Continuing the turn a server restart cut (`RestartContinuation`, `ProviderRuntimeRecoveryService`).
//!
//! With `continueThreadsAfterServerUpdate` on, the server records which root turns are still running
//! while it shuts down (`prepare_for_shutdown`), and again when it starts after a crash (`recover`): each
//! gets a `provider-runtime.continue` effect. The effect sends the source run's thread a
//! `message.dispatch` carrying `restartContinuationOfRunId`; the planner honours it only while the
//! source is still the thread's latest cancelled run (`execution.rs`), and a Codex turn started for it
//! resumes the cut turn instead of being prompted.
//!
//! Not ported: the note of background work the restart cancelled (`RestartBackgroundNote`).
use crate::{
    execution::{checked, event},
    message_queue::latest_executed,
    persistence::{Decision, NewEffect, Store, StoreError, read_projection},
    provider_registry::ProviderRegistry,
};
use chrono::{DateTime, Utc};
use rusqlite::Transaction;
use serde_json::{Value, json};

pub(crate) const CONTINUE_PROMPT: &str = "Continue where you left off.";

fn rows<'a>(value: &'a Value, field: &str) -> &'a [Value] {
    value[field].as_array().map_or(&[], Vec::as_slice)
}

/// `continueThreadsAfterServerUpdate` for a project: its override, else the environment's.
pub(crate) fn enabled(providers: &ProviderRegistry, project_id: &str) -> bool {
    let settings = serde_json::to_value(providers.settings()).unwrap_or(Value::Null);
    settings["projectSettingsOverrides"][project_id]["continueThreadsAfterServerUpdate"].as_bool().unwrap_or(settings["continueThreadsAfterServerUpdate"] == true)
}

/// `restartContinuationRun`: the unfinished root turn a restart would cut, when it can be resumed.
/// Leftover background work is not a reason to wake a settled run.
pub(crate) fn continuation_run(projection: &Value) -> Option<&Value> {
    let thread = &projection["thread"];
    if !thread["archivedAt"].is_null() || !thread["deletedAt"].is_null() {
        return None;
    }
    let run = latest_executed(projection)?;
    let prepared = run["status"] == "starting" && !run["restartContinuationOfRunId"].is_null();
    if run["status"] != "running" && !prepared {
        return None;
    }
    let live_turn_required = !prepared;
    if thread["providerInstanceId"] != run["providerInstanceId"] {
        return None;
    }
    let provider_thread = rows(projection, "providerThreads").iter().find(|row| row["id"] == run["providerThreadId"])?;
    let native = &provider_thread["nativeThreadRef"];
    if provider_thread["appThreadId"] != thread["id"]
        || !provider_thread["ownerNodeId"].is_null()
        || provider_thread["providerInstanceId"] != run["providerInstanceId"]
        || native["nativeId"].as_str().is_none()
        || native["strength"] != "strong"
        || native["driver"] != provider_thread["driver"]
        || (live_turn_required && provider_thread["status"] != "active")
        || matches!(provider_thread["status"].as_str(), Some("closed" | "archived"))
    {
        return None;
    }
    let session = rows(projection, "providerSessions").iter().find(|session| session["id"] == provider_thread["providerSessionId"])?;
    // Most adapters keep a live session "ready" through its turns, so only a stopped or failed one rules out a live turn.
    if session["providerInstanceId"] != run["providerInstanceId"] || session["driver"] != provider_thread["driver"] || (live_turn_required && matches!(session["status"].as_str(), Some("stopped" | "error"))) {
        return None;
    }
    if live_turn_required && !rows(projection, "providerTurns").iter().any(|turn| turn["providerThreadId"] == provider_thread["id"] && turn["runAttemptId"] == run["activeAttemptId"] && turn["status"] == "running") {
        return None;
    }
    Some(run)
}

/// `isRestartNoteSource`: a run whose turn had settled before the restart cancelled its background work.
pub(crate) fn is_note_source(run: &Value, provider_turns: &[Value]) -> bool {
    matches!(run["status"].as_str(), Some("completed" | "waiting" | "cancelled"))
        && run["restartCancelledBackgroundWork"].as_array().is_some_and(|work| !work.is_empty())
        && !provider_turns.iter().any(|turn| turn["runAttemptId"] == run["activeAttemptId"] && turn["status"] == "cancelled")
}

/// `runRanAfter`: whether `run` ran after `other` (an unfinished run is the latest).
fn ran_after(run: &Value, other: &Value) -> bool {
    let end = |run: &Value| run["completedAt"].as_str().and_then(|text| DateTime::parse_from_rfc3339(text).ok()).map_or(i64::MAX, |time| time.timestamp_millis());
    if end(run) == end(other) { run["ordinal"].as_u64() > other["ordinal"].as_u64() } else { end(run) > end(other) }
}

/// Whether a source run still stands for the thread's next turn: it was cancelled by the restart, nothing
/// but queued runs ran after it, and the user did not stop it.
pub(crate) fn source_stands(projection: &Value, source: &Value) -> bool {
    let thread = &projection["thread"];
    // A graceful shutdown ends the turn as interrupted where a crash leaves it cancelled; a user's stop
    // is told apart by its request, checked below.
    matches!(source["status"].as_str(), Some("cancelled" | "interrupted"))
        && !is_note_source(source, rows(projection, "providerTurns"))
        && thread["archivedAt"].is_null()
        && thread["deletedAt"].is_null()
        && thread["providerInstanceId"] == source["providerInstanceId"]
        // Held queued runs never started; they wait behind the continuation.
        && !rows(projection, "runs").iter().any(|run| run["id"] != source["id"] && run["status"] != "queued" && ran_after(run, source))
        && !crate::thread_pull_requests::stop_reached_run(projection, source["id"].as_str().unwrap_or_default())
}

fn continue_effect(run_id: &str, command_id: &str, thread_id: &str, now: DateTime<Utc>) -> NewEffect {
    NewEffect { id: format!("effect:restart-continuation:{run_id}"), command_id: command_id.into(), thread_id: thread_id.into(), request: json!({"type":"provider-runtime.continue","sourceRunId":run_id}), available_at: crate::execution::at(now) }
}

/// Queues the continue effect for one thread, when it has a run to continue. The effect id names the run,
/// so a shutdown and the startup after it ask once.
fn queue(store: &Store, projection: &Value, command_id: &str, now: DateTime<Utc>) -> Result<bool, StoreError> {
    let Some(run) = continuation_run(projection) else { return Ok(false) };
    let (run_id, thread_id) = (run["id"].as_str().unwrap_or_default(), projection["thread"]["id"].as_str().unwrap_or_default());
    let command_id = format!("{command_id}:{run_id}");
    let effect = continue_effect(run_id, &command_id, thread_id, now);
    store.dispatch(&command_id, "thread", thread_id, "provider-runtime.reconcile", now, |_| Ok(Decision::Accepted { events: vec![], effects: vec![effect] }), crate::thread::reduce)?;
    Ok(true)
}

/// Stores whose server is stopping: it must not run the continuations it just queued, since the turns they
/// continue are still running. The next start claims them (`claim_effect_with_options`).
static STOPPING: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());

pub(crate) fn started(store: &Store) {
    STOPPING.lock().unwrap().retain(|identity| *identity != store.identity());
}
pub(crate) fn stopping(store: &Store) -> bool {
    STOPPING.lock().unwrap().contains(&store.identity())
}

/// `prepareForShutdown`: snapshot which turns are live before the providers stop. A provider may finish
/// while this commits; delivery rejects a source run that actually completed.
pub(crate) fn prepare_for_shutdown(store: &Store, providers: &ProviderRegistry) {
    STOPPING.lock().unwrap().push(store.identity());
    let now = Utc::now();
    for projection in store.projections("thread").unwrap_or_default() {
        if !enabled(providers, projection["thread"]["projectId"].as_str().unwrap_or_default()) {
            continue;
        }
        // One failing thread must not cost the threads after it their continuation.
        if let Err(error) = queue(store, &projection, "command:restart-prepare", now) {
            tracing::warn!(thread = projection["thread"]["id"].as_str().unwrap_or_default(), %error, "Failed to prepare a restart continuation");
        }
    }
}

/// The turns a startup finds cut: queue their continuation before recovery cancels them.
pub(crate) fn queue_after_crash(store: &Store, providers: &ProviderRegistry, projections: &[Value]) {
    let now = Utc::now();
    for projection in projections {
        if enabled(providers, projection["thread"]["projectId"].as_str().unwrap_or_default()) {
            if let Err(error) = queue(store, projection, "command:restart-prepare", now) {
                tracing::warn!(thread = projection["thread"]["id"].as_str().unwrap_or_default(), %error, "Failed to queue a restart continuation");
            }
        }
    }
}

/// `continueRestartedRun`: sends the continuation, unless the thread moved on since the effect was queued.
pub(crate) fn execute(store: &Store, providers: &ProviderRegistry, effect: &crate::persistence::Effect) -> Result<(), String> {
    let (thread_id, source_id) = (effect.thread_id.as_str(), effect.request["sourceRunId"].as_str().unwrap_or_default());
    let Some(projection) = store.projection("thread", thread_id).map_err(|error| error.to_string())? else { return Ok(()) };
    if !enabled(providers, projection["thread"]["projectId"].as_str().unwrap_or_default()) {
        return Ok(());
    }
    let message_id = format!("message:restart-continuation:{source_id}");
    if rows(&projection, "messages").iter().any(|message| message["id"] == message_id) {
        return Ok(());
    }
    let Some(source) = rows(&projection, "runs").iter().find(|run| run["id"] == source_id) else { return Ok(()) };
    // Pending effects from older versions may target settled work, and a user's submission after
    // reconciliation takes precedence over an automatic prompt.
    if !source_stands(&projection, source) {
        return Ok(());
    }
    let source_message = rows(&projection, "messages").iter().find(|message| message["id"] == source["userMessageId"]);
    if source_message.is_some_and(|message| message["text"].as_str().is_some_and(|text| text.trim().eq_ignore_ascii_case("/compact"))) {
        return Ok(());
    }
    let command = json!({"type":"message.dispatch","commandId":format!("command:restart-continuation:{source_id}"),"threadId":thread_id,"messageId":message_id,"text":CONTINUE_PROMPT,"attachments":[],"modelSelection":source["modelSelection"],"dispatchMode":{"type":"start_immediately"},"createdBy":"agent","creationSource":"server","restartContinuationOfRunId":source_id});
    let driver = providers.driver(source["providerInstanceId"].as_str().unwrap_or_default()).map_err(|error| error.to_string())?;
    let now = Utc::now();
    store
        .dispatch_resolved(
            command["commandId"].as_str().unwrap_or_default(),
            "thread",
            "message.dispatch",
            now,
            |_, _| Ok(thread_id.to_owned()),
            |transaction: &Transaction<'_>, _| {
                let projection = read_projection(transaction, "thread", thread_id)?.ok_or_else(|| StoreError::InvalidCommand("Thread not found.".into()))?;
                crate::execution::plan_message_in_transaction(&command, &projection, transaction, now, driver)
            },
            crate::thread::reduce,
        )
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// What a run says about the turn it continues: Codex resumes the cut turn natively, so its start sends no input.
pub(crate) fn resumes_natively(run: &Value) -> bool {
    !run["restartContinuationOfRunId"].is_null()
}

/// A no-op thread update the planner answers a stale continuation with, so a delayed delivery is accepted
/// and changes nothing.
pub(crate) fn stale(command: &Value, projection: &Value, now: DateTime<Utc>) -> Result<Decision, StoreError> {
    let thread = &projection["thread"];
    let events = vec![event(thread["id"].as_str().unwrap_or_default(), command["commandId"].as_str().unwrap_or_default(), "thread.metadata-updated", checked::<t3_contracts::AppThread>(thread.clone())?, now)];
    Ok(Decision::Accepted { events, effects: vec![] })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cut_thread() -> Value {
        json!({"thread":{"id":"t","projectId":"p","providerInstanceId":"codex","archivedAt":null,"deletedAt":null},
            "runs":[{"id":"r1","ordinal":1,"status":"running","providerInstanceId":"codex","providerThreadId":"pt","activeAttemptId":"a1","completedAt":null}],
            "providerThreads":[{"id":"pt","appThreadId":"t","ownerNodeId":null,"providerInstanceId":"codex","driver":"codex","status":"active","providerSessionId":"s","nativeThreadRef":{"driver":"codex","nativeId":"n","strength":"strong"}}],
            "providerSessions":[{"id":"s","providerInstanceId":"codex","driver":"codex","status":"ready"}],
            "providerTurns":[{"providerThreadId":"pt","runAttemptId":"a1","status":"running"}],"turnItems":[]})
    }

    #[test]
    fn only_a_live_root_turn_with_a_strong_native_thread_is_continued() {
        assert_eq!(continuation_run(&cut_thread()).unwrap()["id"], "r1");
        for (path, value) in [("/runs/0/status", "completed"), ("/providerThreads/0/status", "idle"), ("/providerSessions/0/status", "stopped"), ("/providerTurns/0/status", "completed"), ("/providerThreads/0/nativeThreadRef/strength", "weak"), ("/thread/providerInstanceId", "other"), ("/thread/archivedAt", "2026-01-01T00:00:00.000Z")] {
            let mut projection = cut_thread();
            *projection.pointer_mut(path).unwrap() = json!(value);
            assert!(continuation_run(&projection).is_none(), "{path}");
        }
        // A prepared continuation needs no live turn.
        let mut prepared = cut_thread();
        prepared["runs"][0]["status"] = json!("starting");
        prepared["runs"][0]["restartContinuationOfRunId"] = json!("r0");
        prepared["providerTurns"] = json!([]);
        prepared["providerThreads"][0]["status"] = json!("idle");
        assert_eq!(continuation_run(&prepared).unwrap()["id"], "r1");
    }

    #[test]
    fn a_source_stands_only_until_the_user_or_another_run_takes_the_thread() {
        let mut projection = cut_thread();
        projection["runs"][0]["status"] = json!("cancelled");
        projection["runs"][0]["completedAt"] = json!("2026-01-01T00:00:00.000Z");
        let source = projection["runs"][0].clone();
        assert!(source_stands(&projection, &source));
        let mut later = projection.clone();
        later["runs"].as_array_mut().unwrap().push(json!({"id":"r2","ordinal":2,"status":"completed","completedAt":"2026-01-01T00:05:00.000Z"}));
        assert!(!source_stands(&later, &source));
        let mut queued = projection.clone();
        queued["runs"].as_array_mut().unwrap().push(json!({"id":"r2","ordinal":2,"status":"queued","completedAt":null}));
        assert!(source_stands(&queued, &source));
        let mut stopped = projection.clone();
        stopped["turnItems"] = json!([{"type":"run_interrupt_request","runId":"r1"}]);
        assert!(!source_stands(&stopped, &source));
    }
}
