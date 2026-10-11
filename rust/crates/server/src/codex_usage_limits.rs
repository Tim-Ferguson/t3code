//! Codex subscription usage as a contract value (`codexUsageLimits.ts`, `codexPlanLabel` of `CodexProvider.ts`). The
//! `account/rateLimits/read` response and the `account/rateLimits/updated` notification carry one snapshot shape, and a usage-limit
//! source (`cliproxy_api.rs`) builds the same shape from a hub's answer, so one mapper serves them all. Snapshots are JSON values
//! with the camelCase names of the Codex app-server, as the other Codex modules read them.
use crate::claude_usage_limits::{clamp_percent, make_usage_limits};
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Value, json};

const SESSION_MINS: f64 = 5.0 * 60.0;
const WEEK_MINS: f64 = 7.0 * 24.0 * 60.0;
const MONTH_MINS: f64 = 30.0 * 24.0 * 60.0;

/// A whole number prints as an integer, as it would after `JSON.stringify`.
fn number(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() < 9e15 { json!(value as i64) } else { json!(value) }
}

/// `isoFromEpochSeconds`: only a positive, finite instant `Date` can hold.
fn iso_from_epoch_seconds(value: &Value) -> Option<String> {
    let seconds = value.as_f64().filter(|value| value.is_finite() && *value > 0.0)?;
    let millis = seconds * 1000.0;
    if millis.abs() > 8.64e15 {
        return None;
    }
    DateTime::<Utc>::from_timestamp_millis(millis as i64).map(|time| time.to_rfc3339_opts(SecondsFormat::Millis, true))
}

fn kind_for_duration(mins: f64) -> (&'static str, &'static str) {
    if mins >= MONTH_MINS {
        ("monthly", "Monthly")
    } else if mins >= WEEK_MINS {
        ("weekly", "Weekly")
    } else {
        ("session", "Session")
    }
}

/// `codexRateLimitsToWindows`: `primary` and `secondary` are positions, not durations. Codex usually sends
/// `windowDurationMins`; when it does not, paid plans expose the 5-hour and weekly pair and Free/Go one monthly allowance.
fn windows(snapshot: &Value) -> Vec<Value> {
    // Only the main allowance: a model-specific notification (such as Spark) must not replace its rows. Older CLIs omit the limit id.
    if snapshot["limitId"].as_str().is_some_and(|id| !id.is_empty() && id != "codex") {
        return vec![];
    }
    let monthly_plan = matches!(snapshot["planType"].as_str(), Some("free" | "go"));
    let mut rows = vec![];
    for (id, position, fallback) in [("primary", "primary", if monthly_plan { MONTH_MINS } else { SESSION_MINS }), ("secondary", "secondary", WEEK_MINS)] {
        let window = &snapshot[position];
        let Some(used) = window["usedPercent"].as_f64().filter(|used| used.is_finite()) else { continue };
        let duration = window["windowDurationMins"].as_f64().unwrap_or(fallback);
        let (kind, label) = kind_for_duration(duration);
        let mut row = json!({"id":id,"kind":kind,"label":label,"usedPercent":number(clamp_percent(used)),"windowDurationMins":number(duration)});
        if let Some(resets_at) = iso_from_epoch_seconds(&window["resetsAt"]) {
            row["resetsAt"] = json!(resets_at);
        }
        rows.push(row);
    }
    rows
}

/// `codexResetCreditsToContract`: the count and the earliest expiry among the credits still available.
pub fn reset_credits_to_contract(summary: &Value) -> Option<Value> {
    if !summary.is_object() {
        return None;
    }
    let earliest = summary["credits"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|credit| credit["status"] == "available")
        .filter_map(|credit| credit["expiresAt"].as_f64())
        .fold(None, |least: Option<f64>, expiry| Some(least.map_or(expiry, |least| least.min(expiry))));
    let mut credits = json!({"availableCount":summary["availableCount"].as_f64().unwrap_or(0.0).max(0.0) as u64});
    if let Some(expires_at) = earliest.and_then(|expiry| iso_from_epoch_seconds(&json!(expiry))) {
        credits["nextExpiresAt"] = json!(expires_at);
    }
    Some(credits)
}

/// `codexRateLimitsToLimits`. `by_limit_id` is the read response's `rateLimitsByLimitId`; its `codex` bucket is the main
/// allowance and wins over the legacy `snapshot`, which can name another limit.
pub fn rate_limits_to_limits(snapshot: &Value, by_limit_id: Option<&Value>, reset_credits: Option<&Value>, checked_at: &str) -> Value {
    let main = by_limit_id.map(|buckets| &buckets["codex"]).filter(|bucket| !bucket.is_null()).unwrap_or(snapshot);
    let mut limits = make_usage_limits(checked_at, windows(main));
    if let Some(credits) = reset_credits.and_then(reset_credits_to_contract) {
        limits["resetCredits"] = credits;
    }
    limits
}

/// `codexRateLimitsToUpdate`: the windows a notification names, or nothing when it names none.
pub fn rate_limits_to_update(snapshot: &Value) -> Option<Value> {
    let windows = windows(snapshot);
    (!windows.is_empty()).then(|| json!({"windows":windows}))
}

/// `codexPlanLabel`: the plan as the provider labels it, shared with usage-limit sources, which report the same `planType` slugs.
pub fn plan_label(plan: Option<&str>) -> Option<&'static str> {
    crate::codex::plan_label(plan)
}
