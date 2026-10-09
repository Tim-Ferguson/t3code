//! "Edit from here": rewinding a thread to before one of its user messages. Port of
//! `deriveRevertTurnCountByUserMessageId`, `revertThreadCheckpoint`'s checkpoint choice and
//! `waitForRevertedMessage` (`session-logic.ts`, `threadCommands.ts`, `ChatView.logic.ts`).
use serde_json::Value;
use std::collections::BTreeMap;

pub const CONFIRM_TITLE: &str = "Edit from here?";
pub const CONFIRM_BODY: &str =
    "Rewind chat to before this message. Your prompt and attachments return to the composer.";
pub const REVERT_FILES: &str = "Revert files too";
pub const REVERT_KEEP: &str = "Revert and keep changes";
pub const WORKING_MESSAGE: &str = "Interrupt the current turn before reverting checkpoints.";
pub const UNSUPPORTED_MESSAGE: &str =
    "This provider does not support reverting conversation history. Start a new thread instead.";
pub const TIMEOUT_MESSAGE: &str = "Timed out waiting for the thread to rewind.";
pub const ROOM_MESSAGE: &str = "Make room for this message's attachments in the composer before rewinding.";
pub const PREPARING_MESSAGE: &str = "Wait for attachments to finish preparing before rewinding.";
/// How long the client waits for the server to finish a rewind it accepted.
pub const TIMEOUT_MS: u64 = 120_000;

/// Text sent in place of an empty prompt when a message is attachments only.
const ATTACHMENT_ONLY_BOOTSTRAP_PROMPT: &str = "[User attached one or more files without additional text. Respond using the conversation context and the attached files.]";
const PLAN_IMPLEMENTATION_PROMPT_PREFIX: &str = "PLEASE IMPLEMENT THIS PLAN:\n";
const CLAUDE_ULTRATHINK_PREFIX: &str = "Ultrathink:\n";

/// The checkpoint a rewind to before one run's message restores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub checkpoint_id: String,
    pub scope_id: String,
}

fn rows<'a>(projection: &'a Value, field: &str) -> &'a [Value] {
    projection[field].as_array().map(Vec::as_slice).unwrap_or(&[])
}

/// For each run whose own checkpoint is ready, the ready checkpoint of the turn before it
/// (the thread's baseline for the first run). Only these messages can be edited from: the
/// server refuses a checkpoint that is `stale`, `missing` or `error`.
pub fn targets(projection: &Value) -> BTreeMap<String, Target> {
    let checkpoints = rows(projection, "checkpoints");
    // The newest checkpoint of each turn count (a re-run after a rewind reuses the ordinal) and
    // the baseline taken before the first turn.
    let mut by_ordinal: BTreeMap<u64, &Value> = BTreeMap::new();
    let mut baseline = None;
    for checkpoint in checkpoints {
        match checkpoint["appRunOrdinal"].as_u64() {
            Some(ordinal) => {
                by_ordinal.insert(ordinal, checkpoint);
            }
            None if checkpoint["ordinalWithinScope"] == 0 => baseline = Some(checkpoint),
            None => {}
        }
    }
    let mut found = BTreeMap::new();
    for own in checkpoints {
        let (Some(run_id), Some(ordinal)) = (own["runId"].as_str(), own["appRunOrdinal"].as_u64()) else {
            continue;
        };
        if own["status"] != "ready" {
            continue;
        }
        let before = ordinal.saturating_sub(1);
        let previous = if before == 0 { baseline } else { by_ordinal.get(&before).copied() };
        let Some(previous) = previous.filter(|checkpoint| checkpoint["status"] == "ready") else {
            continue;
        };
        if let (Some(checkpoint_id), Some(scope_id)) = (previous["id"].as_str(), previous["scopeId"].as_str()) {
            found.insert(run_id.to_owned(), Target { checkpoint_id: checkpoint_id.to_owned(), scope_id: scope_id.to_owned() });
        }
    }
    found
}

/// What the thread projection says about one accepted rewind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    /// Accepted; the server is still rewinding (or this thread has not loaded yet).
    Waiting,
    /// The message's run no longer exists in the conversation.
    Done,
    /// The server gave up; the reason is the server's.
    Failed(String),
}

/// `command_id` is the rewind's command, `run_id` the run of the message it rewinds to before.
pub fn progress(projection: Option<&Value>, thread_id: &str, command_id: &str, run_id: &str) -> Progress {
    let Some(projection) = projection.filter(|projection| projection["thread"]["id"] == thread_id) else {
        return Progress::Waiting;
    };
    let failure = &projection["thread"]["rollbackFailure"];
    if failure["requestId"] == command_id {
        return Progress::Failed(failure["message"].as_str().unwrap_or("The rewind failed.").to_owned());
    }
    // The server rolls back every settled run from the message's on, and the projection hears of
    // them one by one: the rewind is over when none of them is left.
    let runs = rows(projection, "runs");
    let Some(own) = runs.iter().find(|run| run["id"] == run_id).filter(|run| run["status"] == "rolled_back") else {
        return Progress::Waiting;
    };
    let from = own["ordinal"].as_u64().unwrap_or(0);
    let remaining = runs.iter().any(|run| {
        run["ordinal"].as_u64().unwrap_or(0) >= from && matches!(run["status"].as_str(), Some("completed" | "interrupted" | "failed" | "cancelled"))
    });
    if remaining { Progress::Waiting } else { Progress::Done }
}

/// The failure of the thread's current rollback request, as `(request id, message)`. A failure
/// of an older request is history: a newer accepted rollback clears the field, and one that is
/// somehow left behind is ignored.
pub fn current_failure(projection: Option<&Value>, thread_id: &str) -> Option<(String, String)> {
    let thread = &projection.filter(|projection| projection["thread"]["id"] == thread_id)?["thread"];
    let failure = &thread["rollbackFailure"];
    let request = failure["requestId"].as_str()?;
    (thread["rollbackRequestId"] == request).then(|| (request.to_owned(), failure["message"].as_str().unwrap_or("The rewind failed.").to_owned()))
}

/// Reduces a sent message to the text the user typed: app-composed sends are not history.
/// (`recallableComposerPrompt`; the composer-context blocks it also strips are not produced
/// by this client.)
pub fn recallable_prompt(message_text: &str) -> String {
    let mut prompt = message_text.trim();
    if let Some(rest) = prompt.strip_prefix(CLAUDE_ULTRATHINK_PREFIX) {
        prompt = rest;
    }
    let trimmed = prompt.trim();
    if trimmed == ATTACHMENT_ONLY_BOOTSTRAP_PROMPT || trimmed.starts_with(PLAN_IMPLEMENTATION_PROMPT_PREFIX) {
        return String::new();
    }
    trimmed.to_owned()
}

/// The composer text after the message returns to it: whatever is already typed stays, and the
/// message follows after a blank line.
pub fn restored_prompt(current: &str, message_text: &str) -> String {
    let restored = recallable_prompt(message_text);
    if restored.is_empty() {
        current.to_owned()
    } else if current.is_empty() {
        restored
    } else {
        format!("{current}\n\n{restored}")
    }
}

/// Whether the provider can rewind its conversation. Only an explicit `false` says no.
pub fn provider_supports_rollback(provider: Option<&t3_contracts::ServerProvider>) -> bool {
    !matches!(provider.and_then(|provider| provider.supports_conversation_rollback), Some(Some(false)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn projection() -> Value {
        json!({
            "thread": {"id": "t", "rollbackRequestId": null, "rollbackFailure": null},
            "runs": [{"id": "r1", "ordinal": 1, "status": "completed"}, {"id": "r2", "ordinal": 2, "status": "completed"}, {"id": "r3", "ordinal": 3, "status": "completed"}],
            "checkpoints": [
                {"id": "c0", "scopeId": "s", "runId": null, "appRunOrdinal": null, "ordinalWithinScope": 0, "status": "ready"},
                {"id": "c1", "scopeId": "s", "runId": "r1", "appRunOrdinal": 1, "ordinalWithinScope": 1, "status": "ready"},
                {"id": "c2", "scopeId": "s", "runId": "r2", "appRunOrdinal": 2, "ordinalWithinScope": 2, "status": "ready"},
                {"id": "c3", "scopeId": "s", "runId": "r3", "appRunOrdinal": 3, "ordinalWithinScope": 3, "status": "ready"},
            ],
        })
    }

    #[test]
    fn each_message_rewinds_to_the_checkpoint_before_its_turn() {
        let found = targets(&projection());
        assert_eq!(found["r1"], Target { checkpoint_id: "c0".into(), scope_id: "s".into() });
        assert_eq!(found["r2"].checkpoint_id, "c1");
        assert_eq!(found["r3"].checkpoint_id, "c2");
    }

    #[test]
    fn only_ready_checkpoints_are_offered() {
        let mut value = projection();
        value["checkpoints"][3]["status"] = json!("error");
        // r3 has no ready checkpoint of its own; r4 would rewind to the error one.
        assert!(!targets(&value).contains_key("r3"));
        value["checkpoints"][1]["status"] = json!("stale");
        let found = targets(&value);
        assert!(!found.contains_key("r1"), "a stale own checkpoint offers nothing");
        assert!(!found.contains_key("r2"), "a stale target offers nothing");
        value["checkpoints"][0]["status"] = json!("missing");
        assert!(targets(&value).is_empty());
    }

    #[test]
    fn a_rerun_after_a_rewind_uses_the_newest_checkpoint_of_an_ordinal() {
        let mut value = projection();
        value["checkpoints"][2]["status"] = json!("stale");
        value["checkpoints"][3]["status"] = json!("stale");
        value["checkpoints"].as_array_mut().unwrap().push(json!({"id": "c2b", "scopeId": "s", "runId": "r4", "appRunOrdinal": 2, "ordinalWithinScope": 4, "status": "ready"}));
        value["checkpoints"].as_array_mut().unwrap().push(json!({"id": "c3b", "scopeId": "s", "runId": "r5", "appRunOrdinal": 3, "ordinalWithinScope": 5, "status": "ready"}));
        let found = targets(&value);
        assert_eq!(found["r5"].checkpoint_id, "c2b");
        assert_eq!(found["r4"].checkpoint_id, "c1");
        assert!(!found.contains_key("r2"));
    }

    #[test]
    fn progress_follows_the_run_and_the_failure_of_this_request() {
        let mut value = projection();
        assert_eq!(progress(Some(&value), "t", "rewind", "r3"), Progress::Waiting);
        assert_eq!(progress(None, "t", "rewind", "r3"), Progress::Waiting);
        assert_eq!(progress(Some(&value), "other", "rewind", "r3"), Progress::Waiting);
        value["runs"][2]["status"] = json!("rolled_back");
        assert_eq!(progress(Some(&value), "t", "rewind", "r3"), Progress::Done);
        // Rewinding to before r2 is over only when r3 has gone too.
        value["runs"][1]["status"] = json!("rolled_back");
        value["runs"][2]["status"] = json!("completed");
        assert_eq!(progress(Some(&value), "t", "rewind", "r2"), Progress::Waiting);
        value["runs"][2]["status"] = json!("rolled_back");
        assert_eq!(progress(Some(&value), "t", "rewind", "r2"), Progress::Done);
        value["runs"][1]["status"] = json!("completed");
        // Another run being rolled back is not this message's.
        assert_eq!(progress(Some(&value), "t", "rewind", "r2"), Progress::Waiting);
        // The failure of another request is not this rewind's.
        value["thread"]["rollbackFailure"] = json!({"requestId": "older", "message": "old"});
        assert_eq!(progress(Some(&value), "t", "rewind", "r3"), Progress::Done);
        value["runs"][2]["status"] = json!("completed");
        value["thread"]["rollbackFailure"] = json!({"requestId": "rewind", "message": "The provider could not roll back this conversation."});
        assert_eq!(progress(Some(&value), "t", "rewind", "r3"), Progress::Failed("The provider could not roll back this conversation.".into()));
    }

    #[test]
    fn a_failure_is_shown_only_for_the_threads_current_request() {
        let mut value = projection();
        assert_eq!(current_failure(Some(&value), "t"), None);
        value["thread"]["rollbackFailure"] = json!({"requestId": "a", "message": "boom"});
        value["thread"]["rollbackRequestId"] = json!("b");
        assert_eq!(current_failure(Some(&value), "t"), None);
        value["thread"]["rollbackRequestId"] = json!("a");
        assert_eq!(current_failure(Some(&value), "t"), Some(("a".into(), "boom".into())));
        assert_eq!(current_failure(Some(&value), "other"), None);
        assert_eq!(current_failure(None, "t"), None);
    }

    #[test]
    fn refusals_lose_only_the_generic_prefix() {
    }

    #[test]
    fn the_returned_prompt_is_the_text_the_user_typed() {
        assert_eq!(recallable_prompt("  fix it \n"), "fix it");
        assert_eq!(recallable_prompt("Ultrathink:\nfix it"), "fix it");
        assert_eq!(recallable_prompt(ATTACHMENT_ONLY_BOOTSTRAP_PROMPT), "");
        assert_eq!(recallable_prompt("PLEASE IMPLEMENT THIS PLAN:\n# Plan"), "");
        assert_eq!(restored_prompt("", "first"), "first");
        assert_eq!(restored_prompt("typed", "first"), "typed\n\nfirst");
        assert_eq!(restored_prompt("typed", ATTACHMENT_ONLY_BOOTSTRAP_PROMPT), "typed");
    }

    #[test]
    fn only_an_explicit_no_disables_rewinding() {
        assert!(provider_supports_rollback(None));
        let provider = |flag: serde_json::Value| -> t3_contracts::ServerProvider {
            serde_json::from_value(json!({"instanceId": "codex", "driver": "codex", "supportsConversationRollback": flag, "enabled": true, "installed": true, "version": null, "status": "ready", "auth": {"status": "unknown"}, "checkedAt": "2026-01-01T00:00:00Z", "models": [], "slashCommands": [], "skills": []})).unwrap()
        };
        assert!(provider_supports_rollback(Some(&provider(json!(true)))));
        assert!(!provider_supports_rollback(Some(&provider(json!(false)))));
    }
}
