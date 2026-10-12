//! The provider and git counters and timers of the original's `Metrics.ts`, recorded where its `ProviderSessionManager.ts` and
//! `GitVcsDriverCore.ts` record them, for the OTLP metrics exporter (`otlp::count`, `otlp::time`; nothing is recorded before the
//! exporter is installed):
//!
//! - `t3_provider_turns_total` (provider, operation, modelFamily, outcome) for every turn operation, and `t3_provider_turn_duration`
//!   (provider, operation, modelFamily) for the start of a turn only: the time until the adapter accepts the turn, not how long it
//!   runs. The operations are `send`, `steer`, `interrupt` and `runtime-request-response`.
//! - `t3_provider_sessions_total` (provider, operation `open`, outcome) and, for the end of a session, (provider, operation `release`,
//!   reason, outcome).
//! - `t3_git_commands_total` (operation, outcome) and `t3_git_command_duration` (operation), fed by the observer `t3_vcs` takes.
//!
//! Outcomes are `success`, `failure` and `interrupt`. Every value is recorded as a string, and an attribute without a value is left out.
use crate::otlp;
use std::{sync::Arc, time::Duration};

const TURNS: (&str, &str) = ("t3_provider_turns_total", "Total provider turn lifecycle operations.");
const TURN_DURATION: (&str, &str) = ("t3_provider_turn_duration", "Time for the provider adapter to start a turn, not how long the turn runs.");
const SESSIONS: (&str, &str) = ("t3_provider_sessions_total", "Total provider session lifecycle operations.");
const GIT_COMMANDS: (&str, &str) = ("t3_git_commands_total", "Total git commands executed by the server runtime.");
const GIT_DURATION: (&str, &str) = ("t3_git_command_duration", "Git command execution duration.");

/// What a provider effect fails with when the server stops while it runs; the turn it was part of is counted as interrupted.
pub const SERVER_STOPPED: &str = "The server stopped during provider execution.";

/// `normalizeModelMetricLabel`: the family of a model, so no model name is a label.
pub fn model_family(model: Option<&str>) -> Option<&'static str> {
    let normalized = model?.trim().to_lowercase();
    if normalized.is_empty() {
        return None;
    }
    Some(if normalized.contains("gpt") {
        "gpt"
    } else if normalized.contains("claude") {
        "claude"
    } else if normalized.contains("gemini") {
        "gemini"
    } else {
        "other"
    })
}

/// How a turn operation ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Success,
    Failure,
    Interrupt,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Interrupt => "interrupt",
        }
    }
}

fn millis(elapsed: Duration) -> f64 {
    elapsed.as_secs_f64() * 1000.0
}

/// A turn operation of an adapter, recorded when it ends. `model` is the model of the turn that starts (`send`); the others have none.
pub fn record_turn(provider: &str, operation: &str, model: Option<&str>, outcome: Outcome, elapsed: Duration) {
    if !otlp::recording() {
        return;
    }
    let mut attributes = vec![("provider", provider), ("operation", operation)];
    if let Some(family) = model_family(model) {
        attributes.push(("modelFamily", family));
    }
    if operation == "send" {
        otlp::time(TURN_DURATION.0, TURN_DURATION.1, &attributes, millis(elapsed));
    }
    attributes.push(("outcome", outcome.as_str()));
    otlp::count(TURNS.0, TURNS.1, &attributes, 1.0);
}

/// A provider session opened (`operation` `open`) or released (`release`, with the `reason` it ended for).
pub fn record_session(provider: &str, operation: &str, reason: Option<&str>, outcome: Outcome) {
    if !otlp::recording() {
        return;
    }
    let mut attributes = vec![("provider", provider), ("operation", operation)];
    if let Some(reason) = reason {
        attributes.push(("reason", reason));
    }
    attributes.push(("outcome", outcome.as_str()));
    otlp::count(SESSIONS.0, SESSIONS.1, &attributes, 1.0);
}

/// The turn operation an effect asks of an adapter, as the original names it; none for effects that are not turn operations.
pub fn turn_operation(effect_type: &str) -> Option<&'static str> {
    match effect_type {
        "provider-turn.start" => Some("send"),
        "provider-turn.steer" => Some("steer"),
        "provider-turn.interrupt" => Some("interrupt"),
        "runtime-request.respond" => Some("runtime-request-response"),
        _ => None,
    }
}

/// Feeds every git command the crate runs to the counter and the timer. Installed once the metrics exporter starts; the observer
/// itself does nothing while no exporter records.
pub fn install_git_observer() {
    t3_vcs::set_git_command_observer(Some(Arc::new(|command: &t3_vcs::GitCommandObservation| {
        if !otlp::recording() {
            return;
        }
        otlp::time(GIT_DURATION.0, GIT_DURATION.1, &[("operation", &command.operation)], millis(command.elapsed));
        otlp::count(GIT_COMMANDS.0, GIT_COMMANDS.1, &[("operation", &command.operation), ("outcome", command.outcome.as_str())], 1.0);
    })));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_model_is_a_family_and_never_a_name() {
        for (model, family) in [
            (Some("gpt-5.1-codex"), Some("gpt")),
            (Some(" Claude-Opus-4 "), Some("claude")),
            (Some("gemini-2.5-pro"), Some("gemini")),
            (Some("grok-4"), Some("other")),
            (Some("   "), None),
            (Some(""), None),
            (None, None),
        ] {
            assert_eq!(model_family(model), family, "{model:?}");
        }
    }

    #[test]
    fn only_the_effects_the_original_counts_as_turn_operations_are() {
        assert_eq!(
            ["provider-turn.start", "provider-turn.steer", "provider-turn.interrupt", "runtime-request.respond", "provider-turn.restart", "provider-session.detach", "workspace.prepare"].map(turn_operation),
            [Some("send"), Some("steer"), Some("interrupt"), Some("runtime-request-response"), None, None, None]
        );
    }
}
