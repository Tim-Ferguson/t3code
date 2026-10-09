//! The Codex half of thread forks and context handoffs (`CodexAdapterV2.forkThread`,
//! `injectHistory`, `ProviderTurnStartService`): a fork's first run forks the source's native thread
//! at the run it was forked from (`thread/fork` with an inclusive `lastTurnId`), and the handoffs a run
//! carries are injected into the native thread as history (`thread/inject_items`) before its turn
//! starts. A Codex that does not know the method gets the history in the message instead.
use crate::{
    codex_runtime::{commit, projection},
    context_handoff::{self, DeliveryError},
    persistence::Store,
    provider_process::{ProcessError, ProviderProcess},
};
use chrono::Utc;
use serde_json::{Value, json};
use std::time::Duration;

fn rows<'a>(value: &'a Value, field: &str) -> &'a [Value] {
    value[field].as_array().map_or(&[], Vec::as_slice)
}

/// Whether a provider turn is over (`isTerminalProviderTurn`).
fn terminal_turn(turn: &Value) -> bool {
    matches!(turn["status"].as_str(), Some("completed" | "failed" | "interrupted" | "cancelled"))
}

/// The `thread/fork` request for a fork's first run: the source's native thread, cut after the
/// native turn of the run it was forked from. A source turn with no native turn id can only be
/// reached by trimming the fork afterwards, which Codex's legacy history cannot do; that is refused
/// rather than forking more than was asked for.
pub(crate) fn fork_params(store: &Store, transfer: &Value, provider_thread: &Value, cwd: &Value, model: &Value) -> Result<Value, String> {
    let source = projection(store, transfer["sourceThreadId"].as_str().unwrap_or("")).map_err(|error| error.to_string())?;
    let source_thread = rows(&source, "providerThreads")
        .iter()
        .find(|thread| thread["id"] == provider_thread["forkedFrom"]["providerThreadId"])
        .ok_or_else(|| format!("Native fork transfer {} has no source provider execution.", transfer["id"].as_str().unwrap_or("")))?;
    let native = source_thread["nativeThreadRef"]["nativeId"].as_str().ok_or("The source provider thread has no native thread id.")?;
    let source_turns: Vec<&Value> = rows(&source, "providerTurns").iter().filter(|turn| turn["providerThreadId"] == source_thread["id"]).collect();
    let mut params = json!({"threadId":native,"cwd":cwd,"model":model["model"],"config":{"tools.update_plan.enabled":true}});
    if let Some(boundary_id) = provider_thread["forkedFrom"].get("providerTurnId").filter(|id| !id.is_null()) {
        let boundary = source_turns
            .iter()
            .find(|turn| turn["id"] == *boundary_id)
            .ok_or_else(|| format!("Cannot fork Codex thread from provider turn {boundary_id}: source turn was not found in provider thread {}.", source_thread["id"].as_str().unwrap_or("")))?;
        match boundary["nativeTurnRef"]["nativeId"].as_str() {
            Some(native_turn) => params["lastTurnId"] = json!(native_turn),
            None => {
                let ordinal = boundary["ordinal"].as_u64().unwrap_or(0);
                if source_turns.iter().any(|turn| turn["ordinal"].as_u64().unwrap_or(0) > ordinal && terminal_turn(turn)) {
                    return Err(format!("Cannot fork Codex thread {native} at provider turn {boundary_id}: the source turn has no native Codex turn reference, so the fork cannot be cut there."));
                }
            }
        }
    }
    Ok(params)
}

/// What was left to put in the message after the handoffs were delivered.
#[derive(Default)]
pub(crate) struct Inline {
    /// History a Codex that cannot take an injection reads in the message, ahead of it.
    pub context: String,
    /// The handoffs to record as delivered inline once the turn has started.
    pub rows: Vec<Value>,
}

/// Delivers the handoffs ready for `run` into its native thread (`deliverContextHandoffs`): recorded
/// as pending, injected, recorded as injected. A failure after the pending mark leaves it for the
/// next turn to find, which then replaces the native thread rather than guessing.
pub(crate) async fn deliver_handoffs(store: &Store, thread_id: &str, process: &ProviderProcess, snapshot: &Value, run: &Value, native_thread_id: &str, user_text: &str, attachments: &Value) -> Result<Inline, String> {
    let handoffs: Vec<Value> = rows(snapshot, "contextHandoffs").iter().filter(|handoff| handoff["status"] == "ready" && handoff["targetRunId"] == run["id"]).cloned().collect();
    if handoffs.is_empty() {
        return Ok(Inline::default());
    }
    let mut provider_thread = rows(snapshot, "providerThreads").iter().find(|thread| thread["id"] == run["providerThreadId"]).cloned().ok_or("The run has no provider thread.")?;
    provider_thread["nativeThreadRef"] = json!({"driver":"codex","nativeId":native_thread_id,"strength":"strong"});
    // Without a measured occupancy, the saved text of the conversation stands in for it.
    let estimate = context_handoff::native_context_estimate(snapshot, &provider_thread, &run["id"]);
    let budget = context_handoff::handoff_budget(context_handoff::token_cap(), user_text, attachments, &provider_thread, estimate, None);
    let delivered = context_handoff::delivered_item_ids(snapshot, &provider_thread);
    let delivery = match context_handoff::plan_delivery(&handoffs, &provider_thread, budget, false, true, &delivered) {
        Ok(Some(delivery)) => delivery,
        Ok(None) => return Ok(Inline::default()),
        Err(error @ (DeliveryError::Budget | DeliveryError::Uncertain)) => return Err(error.to_string()),
    };
    let record = |status: &str| {
        let stamped = delivery.with_delivery(status);
        commit(store, thread_id, |_| {
            let now = crate::execution::at(Utc::now());
            Ok(stamped.into_iter().map(|mut row| {
                row["updatedAt"] = json!(now);
                ("context-handoff.updated", row)
            }).collect())
        })
    };
    record("pending").map_err(|error| error.to_string())?;
    let injected = process.request("thread/inject_items", json!({"threadId":native_thread_id,"items":delivery.response_items()}), Duration::from_secs(30)).await;
    match injected {
        Ok(_) => {
            record("injected").map_err(|error| error.to_string())?;
            Ok(Inline::default())
        }
        // An app server that predates injection rejects the method before changing anything.
        Err(ProcessError::Remote { code: -32601, .. }) => Ok(Inline { context: delivery.inline_context(), rows: delivery.with_delivery("inline") }),
        Err(error) => Err(error.to_string()),
    }
}
