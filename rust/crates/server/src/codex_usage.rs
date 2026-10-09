//! Codex token usage and subscription rate limits as the original adapter keeps them
//! (`CodexTurnTokenUsage.ts`, `codexUsageLimits.ts`, and the usage helpers of `CodexAdapterV2.ts`).
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Value, json};
use std::collections::HashMap;

fn count(value: &Value) -> u64 {
    value.as_f64().map_or(0, |number| number.max(0.0) as u64)
}

#[derive(Clone, Copy, Default, PartialEq)]
struct Breakdown {
    input: u64,
    cached: u64,
    cache_creation: Option<u64>,
    output: u64,
    reasoning: u64,
}
fn breakdown(usage: &Value) -> Breakdown {
    Breakdown {
        input: count(&usage["inputTokens"]),
        cached: count(&usage["cachedInputTokens"]),
        cache_creation: usage.get("cacheWriteInputTokens").filter(|value| !value.is_null()).map(count),
        output: count(&usage["outputTokens"]),
        reasoning: count(&usage["reasoningOutputTokens"]),
    }
}

/// Usage added by one `thread/tokenUsage/updated`: the growth of the running `total`, or `last`
/// when there is no earlier total (first update after a resume) or Codex reset the total.
fn delta(previous: Option<Breakdown>, current: Breakdown, last: Breakdown) -> Breakdown {
    let Some(previous) = previous else { return last };
    if current.input < previous.input || current.cached < previous.cached || current.output < previous.output || current.reasoning < previous.reasoning {
        return last;
    }
    Breakdown {
        input: current.input - previous.input,
        cached: current.cached - previous.cached,
        cache_creation: match (current.cache_creation, previous.cache_creation) {
            (Some(current), Some(previous)) if current >= previous => Some(current - previous),
            _ => None,
        },
        output: current.output - previous.output,
        reasoning: current.reasoning - previous.reasoning,
    }
}

struct Accumulator {
    input: u64,
    cached: u64,
    cache_creation: Option<u64>,
    output: u64,
    reasoning: u64,
    observed: bool,
    /// The turn spawned or waited on subagents.
    has_subagents: bool,
}

/// One per native thread: the baseline is thread-wide, the accumulators are per turn.
#[derive(Default)]
pub(crate) struct TurnTokenUsage {
    baseline: Option<Breakdown>,
    active_turn: Option<String>,
    by_turn: HashMap<String, Accumulator>,
}
impl TurnTokenUsage {
    pub(crate) fn begin(&mut self, turn_id: &str) {
        self.active_turn = Some(turn_id.to_owned());
    }
    pub(crate) fn accumulate(&mut self, turn_id: &str, usage: &Value) {
        let current = breakdown(&usage["total"]);
        if self.active_turn.as_deref() != Some(turn_id) {
            // The total is thread-wide, so a late update for a finished turn only moves the baseline.
            self.baseline = Some(current);
            return;
        }
        let growth = delta(self.baseline, current, breakdown(&usage["last"]));
        self.baseline = Some(current);
        let entry = self.by_turn.entry(turn_id.to_owned()).or_insert(Accumulator { input: 0, cached: 0, cache_creation: Some(0), output: 0, reasoning: 0, observed: false, has_subagents: false });
        if growth.input > 0 || growth.cached > 0 || growth.output > 0 || growth.reasoning > 0 {
            entry.observed = true;
        }
        entry.input += growth.input;
        entry.cached += growth.cached;
        entry.output += growth.output;
        entry.reasoning += growth.reasoning;
        match (growth.cache_creation, entry.cache_creation) {
            (None, _) => entry.cache_creation = None,
            (Some(growth), Some(total)) => entry.cache_creation = Some(total + growth),
            _ => {}
        }
    }
    /// `markSubagentUsage`: the turn's record says it used subagents.
    pub(crate) fn mark_subagents(&mut self, turn_id: &str) {
        self.by_turn.entry(turn_id.to_owned()).or_insert(Accumulator { input: 0, cached: 0, cache_creation: Some(0), output: 0, reasoning: 0, observed: false, has_subagents: false }).has_subagents = true;
    }
    /// The turn's `TurnTokenUsage` record; usage Codex never reported is `unavailable`.
    pub(crate) fn complete(&mut self, turn_id: &str, completed: bool) -> Value {
        let usage = self.by_turn.remove(turn_id);
        if self.active_turn.as_deref() == Some(turn_id) {
            self.active_turn = None;
        }
        let has_subagents = usage.as_ref().is_some_and(|usage| usage.has_subagents);
        let Some(usage) = usage.filter(|usage| usage.observed) else {
            return json!({"usageStatus":"unavailable","usageScope":"main_agent","hasSubagents":has_subagents});
        };
        let mut record = json!({
            "usageStatus": if completed { "complete" } else { "partial" },
            "usageScope": "main_agent",
            "inputTokens": usage.input,
            "cachedInputTokens": usage.cached.min(usage.input),
            "outputTokens": usage.output,
            "reasoningTokens": usage.reasoning.min(usage.output),
            "hasSubagents": usage.has_subagents,
        });
        if let Some(creation) = usage.cache_creation {
            record["cacheCreationTokens"] = json!(creation.min(usage.input));
        }
        record
    }
}

/// The live context usage a provider turn carries (`codexProviderTurnTokenUsage`).
pub(crate) fn provider_turn_token_usage(usage: &Value, updated_at: &str) -> Value {
    let last = &usage["last"];
    json!({
        "usedTokens": count(&last["totalTokens"]),
        "maxTokens": usage.get("modelContextWindow").filter(|value| !value.is_null()).map(count),
        "inputTokens": count(&last["inputTokens"]),
        "cachedInputTokens": count(&last["cachedInputTokens"]),
        "outputTokens": count(&last["outputTokens"]),
        "reasoningOutputTokens": count(&last["reasoningOutputTokens"]),
        "updatedAt": updated_at,
    })
}

// --- rate limits -----------------------------------------------------------------------------
/// A partial account snapshot: a field an update omits keeps the value seen earlier, and a
/// model-specific snapshot (not the `codex` limit) never replaces the main allowance.
pub(crate) fn merge_rate_limits(previous: Option<&Value>, update: &Value) -> Option<Value> {
    if update["limitId"].as_str().is_some_and(|id| !id.is_empty() && id != "codex") {
        return previous.cloned();
    }
    let Some(previous) = previous else { return Some(update.clone()) };
    let mut merged = previous.clone();
    for field in ["limitId", "planType", "rateLimitReachedType", "primary", "secondary"] {
        if let Some(value) = update.get(field) {
            merged[field] = value.clone();
        }
    }
    Some(merged)
}

fn iso(seconds: &Value) -> Option<String> {
    let seconds = seconds.as_f64().filter(|seconds| seconds.is_finite() && *seconds > 0.0)?;
    DateTime::<Utc>::from_timestamp_millis((seconds * 1000.0) as i64).map(|time| time.to_rfc3339_opts(SecondsFormat::Millis, true))
}

/// `codexUsageLimitResetAt`: all exhausted windows must reset before a continuation can run.
pub(crate) fn usage_limit_reset_at(snapshot: Option<&Value>) -> Option<String> {
    let snapshot = snapshot?;
    if snapshot["limitId"].as_str().is_some_and(|id| !id.is_empty() && id != "codex") {
        return None;
    }
    let exhausted: Vec<Option<String>> = ["primary", "secondary"]
        .iter()
        .filter_map(|position| snapshot.get(*position).filter(|window| window.is_object()))
        .filter(|window| window["usedPercent"].as_f64().is_some_and(|percent| percent.is_finite() && percent.clamp(0.0, 100.0) >= 100.0))
        .map(|window| iso(&window["resetsAt"]))
        .collect();
    if exhausted.is_empty() || exhausted.iter().any(Option::is_none) {
        return None;
    }
    exhausted.into_iter().flatten().max_by_key(|time| DateTime::parse_from_rfc3339(time).map(|time| time.timestamp_millis()).unwrap_or(i64::MIN))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(total: [u64; 4], last: [u64; 4]) -> Value {
        let make = |values: [u64; 4]| json!({"inputTokens":values[0],"cachedInputTokens":values[1],"outputTokens":values[2],"reasoningOutputTokens":values[3]});
        json!({"total":make(total),"last":make(last),"modelContextWindow":1000})
    }

    #[test]
    fn a_turn_counts_the_growth_of_the_running_total_and_a_reset_total_counts_last() {
        let mut state = TurnTokenUsage::default();
        state.begin("turn");
        state.accumulate("turn", &usage([100, 40, 10, 2], [100, 40, 10, 2]));
        state.accumulate("turn", &usage([250, 90, 30, 5], [150, 50, 20, 3]));
        // Codex reset its running total: the newest response is the delta.
        state.accumulate("turn", &usage([20, 0, 4, 0], [20, 0, 4, 0]));
        let record = state.complete("turn", true);
        assert_eq!((record["inputTokens"].as_u64(), record["cachedInputTokens"].as_u64(), record["outputTokens"].as_u64(), record["reasoningTokens"].as_u64()), (Some(270), Some(90), Some(34), Some(5)));
        assert_eq!(record["usageStatus"], "complete");
        assert_eq!(state.complete("turn", true)["usageStatus"], "unavailable");
    }

    #[test]
    fn a_late_update_for_a_finished_turn_only_moves_the_baseline() {
        let mut state = TurnTokenUsage::default();
        state.accumulate("old", &usage([100, 0, 10, 0], [100, 0, 10, 0]));
        state.begin("turn");
        state.accumulate("turn", &usage([130, 0, 15, 0], [30, 0, 5, 0]));
        let record = state.complete("turn", false);
        assert_eq!((record["inputTokens"].as_u64(), record["usageStatus"].as_str()), (Some(30), Some("partial")));
    }

    #[test]
    fn rate_limit_updates_merge_and_the_reset_waits_for_every_exhausted_window() {
        let first = merge_rate_limits(None, &json!({"limitId":"codex","primary":{"usedPercent":100,"resetsAt":1_800_000_000},"secondary":{"usedPercent":100,"resetsAt":1_800_100_000}})).unwrap();
        let merged = merge_rate_limits(Some(&first), &json!({"limitId":"codex","rateLimitReachedType":"x"})).unwrap();
        assert_eq!(merged["primary"], first["primary"]);
        assert_eq!(usage_limit_reset_at(Some(&merged)).as_deref(), Some("2027-01-16T11:46:40.000Z"));
        // A model-specific snapshot never replaces the main allowance.
        assert_eq!(merge_rate_limits(Some(&merged), &json!({"limitId":"spark","primary":{"usedPercent":1}})).unwrap(), merged);
        // An exhausted window without a reset time makes the reset unknown.
        assert_eq!(usage_limit_reset_at(Some(&json!({"primary":{"usedPercent":100}}))), None);
    }
}
