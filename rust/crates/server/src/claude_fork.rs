//! The Claude half of thread forks and context handoffs (`ClaudeAdapterV2.forkThread`, the text
//! delivery of `ContextHandoffDelivery`). The original forks the source's session file with the SDK's
//! `forkSession` and then resumes the copy. This port has the CLI do both in one launch: the fork's
//! first turn resumes the source session with `--fork-session`, cut after the assistant message of the
//! run it was forked from (`--resume-session-at`), and the session id Claude reports in its `init`
//! frame is the fork's native thread. No session file is read or written here. Claude takes no native
//! history injection, so the handoffs a run carries are written ahead of its message; `Inline`,
//! `plan_inline` and `with_context` are that text delivery, shared with the ACP adapter.
use crate::{
    codex_runtime::{commit, projection},
    context_handoff::{self, Delivery, DeliveryError},
    persistence::Store,
};
use chrono::Utc;
use serde_json::{Value, json};

fn rows<'a>(value: &'a Value, field: &str) -> &'a [Value] {
    value[field].as_array().map_or(&[], Vec::as_slice)
}

/// Claude's own turn ids are the assistant message the turn ended with; one the port made up for a
/// turn it has no message for starts with `turn:` (`isSyntheticClaudeTurnId`).
fn synthetic(native_turn_id: &str) -> bool {
    native_turn_id.starts_with("turn:")
}

fn terminal_turn(turn: &Value) -> bool {
    matches!(turn["status"].as_str(), Some("completed" | "failed" | "interrupted" | "cancelled"))
}

/// What a fork's first launch resumes from.
pub(crate) struct ForkRequest {
    /// The source's session, resumed and then forked.
    pub source_session: String,
    /// The assistant message the fork is cut after; none forks the session's head.
    pub up_to_message: Option<String>,
}

/// `resolveClaudeForkUpToMessageId` and the source session of `forkThread`. A boundary turn whose id is
/// one the port made up can be forked at the head only when nothing finished after it; otherwise the
/// fork would contain more than was asked for, and is refused.
pub(crate) fn fork_request(store: &Store, transfer: &Value, provider_thread: &Value) -> Result<ForkRequest, String> {
    let source = projection(store, transfer["sourceThreadId"].as_str().unwrap_or("")).map_err(|error| error.to_string())?;
    let source_thread = rows(&source, "providerThreads")
        .iter()
        .find(|thread| thread["id"] == provider_thread["forkedFrom"]["providerThreadId"])
        .ok_or_else(|| format!("Native fork transfer {} has no source provider execution.", transfer["id"].as_str().unwrap_or("")))?;
    let source_session = source_thread["nativeThreadRef"]["nativeId"]
        .as_str()
        .ok_or_else(|| format!("Provider thread {} has a Claude native conversation without a native id.", source_thread["id"].as_str().unwrap_or("")))?
        .to_owned();
    let Some(boundary_id) = provider_thread["forkedFrom"].get("providerTurnId").filter(|id| !id.is_null()) else {
        return Ok(ForkRequest { source_session, up_to_message: None });
    };
    let mut turns: Vec<&Value> = rows(&source, "providerTurns").iter().filter(|turn| turn["providerThreadId"] == source_thread["id"]).collect();
    turns.sort_by_key(|turn| turn["ordinal"].as_u64());
    let index = turns
        .iter()
        .position(|turn| turn["id"] == *boundary_id)
        .ok_or_else(|| format!("Cannot fork Claude thread from provider turn {boundary_id}: source turn was not found in provider thread {}.", source_thread["id"].as_str().unwrap_or("")))?;
    match turns[index]["nativeTurnRef"]["nativeId"].as_str() {
        Some(native) if !synthetic(native) => Ok(ForkRequest { source_session, up_to_message: Some(native.to_owned()) }),
        _ if turns[index + 1..].iter().any(|turn| terminal_turn(turn)) => Err(format!(
            "Cannot fork Claude thread from prior provider turn {boundary_id}: no SDK assistant message cursor was recorded for that turn."
        )),
        _ => Ok(ForkRequest { source_session, up_to_message: None }),
    }
}

/// A fork's first launch waiting for Claude to say which session it created.
pub(crate) struct ForkWait {
    pub source_session: String,
    pub run_id: String,
    pub transfer_id: Value,
    pub provider_thread_id: Value,
}

/// The rows to commit once Claude reports the fork's session: the provider thread names it, and the
/// transfer is resolved natively (`context-transfer.updated` consumed).
pub(crate) fn adopt(view: &Value, wait: &ForkWait, instance: &str, session: &str) -> Result<Vec<(&'static str, Value)>, String> {
    let mut thread = rows(view, "providerThreads").iter().find(|thread| thread["id"] == wait.provider_thread_id).cloned().ok_or("The fork has no provider thread.")?;
    let native = json!({"driver":"claudeAgent","nativeId":session,"strength":"strong"});
    thread["nativeThreadRef"] = native.clone();
    thread["updatedAt"] = json!(crate::execution::at(Utc::now()));
    let mut events = vec![("provider-thread.updated", thread)];
    if let Some(transfer) = rows(view, "contextTransfers").iter().find(|transfer| transfer["id"] == wait.transfer_id) {
        events.push(("context-transfer.updated", crate::context_transfer::consume_native_fork(transfer, instance, &wait.run_id, &native, Utc::now())));
    }
    Ok(events)
}

/// The handoffs a run delivers in its message.
pub(crate) struct Inline {
    pub context: String,
    delivery: Delivery,
}

impl Inline {
    /// Recorded before the message is written, so a crash after it is found by the next turn.
    pub(crate) fn mark(&self, store: &Store, thread_id: &str, status: &str) -> Result<(), String> {
        let stamped = self.delivery.with_delivery(status);
        commit(store, thread_id, |_| {
            let now = crate::execution::at(Utc::now());
            Ok(stamped
                .into_iter()
                .map(|mut row| {
                    row["updatedAt"] = json!(now);
                    ("context-handoff.updated", row)
                })
                .collect())
        })
        .map_err(|error| error.to_string())
    }
}

/// The history this run's handoffs put ahead of the user's message, within the budget
/// (`deliverContextHandoffs` without native injection). `None` when nothing is waiting.
pub(crate) fn plan_inline(snapshot: &Value, run: &Value, user_text: &str, attachments: &Value) -> Result<Option<Inline>, String> {
    let handoffs: Vec<Value> = rows(snapshot, "contextHandoffs").iter().filter(|handoff| handoff["status"] == "ready" && handoff["targetRunId"] == run["id"]).cloned().collect();
    if handoffs.is_empty() {
        return Ok(None);
    }
    let provider_thread = rows(snapshot, "providerThreads").iter().find(|thread| thread["id"] == run["providerThreadId"]).ok_or("The run has no provider thread.")?;
    let estimate = context_handoff::native_context_estimate(snapshot, provider_thread, &run["id"]);
    let budget = context_handoff::handoff_budget(context_handoff::token_cap(), user_text, attachments, provider_thread, estimate, None);
    let delivered = context_handoff::delivered_item_ids(snapshot, provider_thread);
    match context_handoff::plan_delivery(&handoffs, provider_thread, budget, false, false, &delivered) {
        Ok(Some(delivery)) => Ok(Some(Inline { context: delivery.inline_context(), delivery })),
        Ok(None) => Ok(None),
        Err(error @ (DeliveryError::Budget | DeliveryError::Uncertain)) => Err(error.to_string()),
    }
}

/// The message as the user's text behind the history that goes with it.
pub(crate) fn with_context(context: &str, user_text: &str) -> String {
    if context.is_empty() { user_text.to_owned() } else { format!("{context}\n\nUser message:\n{user_text}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_history_goes_ahead_of_the_users_message() {
        assert_eq!(with_context("", "hi"), "hi");
        assert_eq!(with_context("history", "hi"), "history\n\nUser message:\nhi");
        assert!(synthetic("turn:attempt:1") && !synthetic("68baa075-b7d4-4689-866b-96bd51e570c3"));
    }
}
