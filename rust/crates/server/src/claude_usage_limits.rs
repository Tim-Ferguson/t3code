//! Claude Code subscription usage windows: `claudeUsageLimits.ts` and the window-merging half of
//! `providerUsageLimits.ts`. Two sources produce windows with the same ids, so a streamed
//! `rate_limit_event` lands on the row the `get_usage` probe drew:
//!
//! - `get_usage` (during the capabilities probe) reports every window at once as 0-100 percentages
//!   with ISO reset times;
//! - `rate_limit_event` (streamed during a turn) names one window at a time with a 0-1 fraction and
//!   an epoch-seconds reset.
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Value, json};

const SESSION_MINS: u64 = 5 * 60;
const WEEK_MINS: u64 = 7 * 24 * 60;
const OVERAGE_INCLUDED_EVENT_TYPE: &str = "seven_day_overage_included";

/// The name the probe saw for the overage-included bucket. The streamed event names it by type,
/// so the probe records it and the event mapper reuses it; an event before any probe is dropped.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ScopedLimitNames {
    pub overage_included: Option<String>,
}

/// The overage-included bucket name each instance's last probe saw. Process-wide so the adapter
/// actor, which outlives a refresh's replacement of the instance, reads what the latest probe drew.
fn names_by_instance() -> &'static std::sync::Mutex<std::collections::HashMap<String, ScopedLimitNames>> {
    static NAMES: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, ScopedLimitNames>>> = std::sync::OnceLock::new();
    NAMES.get_or_init(Default::default)
}
pub fn record_scoped_names(instance_id: &str, names: ScopedLimitNames) {
    names_by_instance().lock().unwrap().insert(instance_id.to_owned(), names);
}
pub fn scoped_names(instance_id: &str) -> ScopedLimitNames {
    names_by_instance().lock().unwrap().get(instance_id).cloned().unwrap_or_default()
}

pub fn clamp_percent(value: f64) -> f64 {
    if value.is_finite() { value.clamp(0.0, 100.0) } else { 0.0 }
}

/// A JSON number that is a whole number prints as an integer, as it would after `JSON.stringify`.
fn number(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() < 9e15 { json!(value as i64) } else { json!(value) }
}

fn iso_from_epoch_seconds(value: Option<f64>) -> Option<String> {
    let seconds = value.filter(|value| value.is_finite() && *value > 0.0)?;
    let millis = seconds * 1000.0;
    if millis.abs() > 8.64e15 {
        return None;
    }
    DateTime::<Utc>::from_timestamp_millis(millis as i64).map(|time| time.to_rfc3339_opts(SecondsFormat::Millis, true))
}
fn iso_from_string(value: Option<&str>) -> Option<String> {
    let value = value.filter(|value| !value.is_empty())?;
    DateTime::parse_from_rfc3339(value).ok().map(|time| time.with_timezone(&Utc).to_rfc3339_opts(SecondsFormat::Millis, true))
}

fn window(id: &str, kind: &str, label: &str, minutes: u64, used_percent: f64, resets_at: Option<String>) -> Value {
    let mut window = json!({"id":id,"kind":kind,"label":label,"windowDurationMins":minutes,"usedPercent":number(clamp_percent(used_percent))});
    if let Some(resets_at) = resets_at {
        window["resetsAt"] = json!(resets_at);
    }
    window
}
fn account_window(id: &str, used_percent: f64, resets_at: Option<String>) -> Option<Value> {
    Some(match id {
        "five_hour" => window(id, "session", "Session", SESSION_MINS, used_percent, resets_at),
        "seven_day" => window(id, "weekly", "Weekly", WEEK_MINS, used_percent, resets_at),
        _ => return None,
    })
}
fn scoped_window_id(display_name: &str) -> String {
    let lowered = display_name.to_lowercase();
    let mut id = String::from("seven_day_");
    let mut gap = false;
    for character in lowered.chars() {
        if character.is_ascii_lowercase() || character.is_ascii_digit() {
            if gap {
                id.push('_');
                gap = false;
            }
            id.push(character);
        } else {
            gap = true;
        }
    }
    if gap {
        id.push('_');
    }
    id
}
fn scoped_window(display_name: &str, used_percent: f64, resets_at: Option<String>) -> Value {
    window(&scoped_window_id(display_name), "weekly", &format!("Weekly · {display_name}"), WEEK_MINS, used_percent, resets_at)
}

/// `claudeRateLimitEventToUpdate`: utilization is a 0-1 fraction on the streamed event.
pub fn rate_limit_event_to_update(info: &Value, names: &ScopedLimitNames) -> Option<Value> {
    let kind = info["rateLimitType"].as_str().filter(|kind| !kind.is_empty())?;
    let used = info["utilization"].as_f64()? * 100.0;
    let resets_at = iso_from_epoch_seconds(info["resetsAt"].as_f64());
    if let Some(window) = account_window(kind, used, resets_at.clone()) {
        return Some(json!({"windows":[window]}));
    }
    if kind == OVERAGE_INCLUDED_EVENT_TYPE {
        if let Some(name) = names.overage_included.as_deref().filter(|name| !name.is_empty()) {
            return Some(json!({"windows":[scoped_window(name, used, resets_at)]}));
        }
    }
    None
}

const KIND_ORDER: [&str; 4] = ["session", "weekly", "monthly", "other"];
/// `makeUsageLimits`: windows ordered by kind, then id.
pub fn make_usage_limits(checked_at: &str, windows: Vec<Value>) -> Value {
    let mut windows = windows;
    let rank = |window: &Value| KIND_ORDER.iter().position(|kind| window["kind"] == *kind).unwrap_or(KIND_ORDER.len());
    windows.sort_by(|left, right| rank(left).cmp(&rank(right)).then_with(|| left["id"].as_str().unwrap_or("").cmp(right["id"].as_str().unwrap_or(""))));
    json!({"checkedAt":checked_at,"windows":windows})
}
/// `makeUnavailableUsageLimits`
pub fn make_unavailable_usage_limits(checked_at: &str, reason: &str, message: Option<&str>) -> Value {
    let mut unavailable = json!({"reason":reason});
    if let Some(message) = message.filter(|message| !message.is_empty()) {
        unavailable["message"] = json!(message);
    }
    json!({"checkedAt":checked_at,"windows":[],"unavailable":unavailable})
}

/// `claudeUsageResponseToLimits`: percentages on the `get_usage` response are already 0-100. Also
/// yields the scoped-bucket name the response carried, for the event mapper to reuse.
pub fn usage_response_to_limits(response: &Value, checked_at: &str) -> (Value, ScopedLimitNames) {
    let limits = &response["rate_limits"];
    if response["rate_limits_available"].as_bool() != Some(true) || limits.is_null() || limits == &json!(false) || limits == &json!(0) || limits == &json!("") {
        return (make_unavailable_usage_limits(checked_at, "unsupported", None), ScopedLimitNames::default());
    }
    let mut windows = vec![];
    for id in ["five_hour", "seven_day"] {
        let entry = &limits[id];
        if let Some(used) = entry["utilization"].as_f64() {
            windows.extend(account_window(id, used, iso_from_string(entry["resets_at"].as_str())));
        }
    }
    let mut overage_included = None;
    for entry in limits["model_scoped"].as_array().into_iter().flatten() {
        let Some(name) = entry["display_name"].as_str() else { continue };
        let Some(used) = entry["utilization"].as_f64() else { continue };
        windows.push(scoped_window(name, used, iso_from_string(entry["resets_at"].as_str())));
        // Only a bucket that drew a row may receive events.
        overage_included.get_or_insert_with(|| name.to_owned());
    }
    (make_usage_limits(checked_at, windows), ScopedLimitNames { overage_included })
}

/// `applyUsageLimitsUpdate`: windows upsert by id; one the update omits keeps its values, and a
/// window arriving without a reset or duration keeps what the last probe resolved. An
/// `unsupported` snapshot stays unsupported.
pub fn apply_usage_limits_update(previous: Option<&Value>, update: &Value, checked_at: &str) -> Option<Value> {
    let windows = update["windows"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    if windows.is_empty() || previous.is_some_and(|previous| previous["unavailable"]["reason"] == "unsupported") {
        return previous.cloned();
    }
    let mut merged: Vec<Value> = previous.and_then(|previous| previous["windows"].as_array()).cloned().unwrap_or_default();
    let mut changed = false;
    for window in windows {
        let existing = merged.iter().position(|candidate| candidate["id"] == window["id"]);
        let mut next = window.clone();
        next["usedPercent"] = number(clamp_percent(window["usedPercent"].as_f64().unwrap_or(0.0)));
        if let Some(existing) = existing.map(|index| &merged[index]) {
            for key in ["resetsAt", "windowDurationMins"] {
                if window.get(key).is_none_or(Value::is_null) && !existing[key].is_null() {
                    next[key] = existing[key].clone();
                }
            }
        }
        match existing {
            Some(index) if merged[index] == next => {}
            Some(index) => {
                merged[index] = next;
                changed = true;
            }
            None => {
                merged.push(next);
                changed = true;
            }
        }
    }
    if !changed && previous.is_some_and(|previous| previous["unavailable"].is_null()) {
        return previous.cloned();
    }
    let mut limits = make_usage_limits(checked_at, merged);
    if let Some(credits) = previous.and_then(|previous| previous.get("resetCredits")) {
        limits["resetCredits"] = credits.clone();
    }
    Some(limits)
}

/// `resolveUsageLimitsAfterProbe`: a probe that failed this time must not wipe bars a previous
/// probe or a turn already established; `unsupported` is authoritative and replaces them.
pub fn resolve_usage_limits_after_probe(published: Option<&Value>, probed: Option<&Value>) -> Option<Value> {
    if let (Some(published), Some(probed)) = (published, probed) {
        if probed["unavailable"]["reason"] == "probeFailed" && published["unavailable"].is_null() {
            return Some(published.clone());
        }
    }
    probed.cloned()
}

/// `describeClaudeUsageLimit`: the wait is rendered here, as a duration, so it reads the same on
/// clients in any timezone.
pub fn describe_usage_limit(info: &Value, now_ms: f64, names: &ScopedLimitNames) -> String {
    let label = match info["rateLimitType"].as_str() {
        Some("seven_day_overage_included") if names.overage_included.as_deref().is_some_and(|name| !name.is_empty()) => Some(format!("7-day {}", names.overage_included.as_deref().unwrap())),
        Some("five_hour") => Some("5-hour".to_owned()),
        Some("seven_day") => Some("7-day".to_owned()),
        Some("seven_day_opus") => Some("7-day Opus".to_owned()),
        Some("seven_day_sonnet") => Some("7-day Sonnet".to_owned()),
        Some("seven_day_overage_included") => Some("7-day model".to_owned()),
        Some("overage") => Some("overage".to_owned()),
        _ => None,
    };
    let wait = info["resetsAt"].as_f64().map(|seconds| seconds * 1000.0 - now_ms).filter(|wait| now_ms.is_finite() && *wait > 0.0 && *wait <= 30.0 * 24.0 * 60.0 * 60.0 * 1000.0).map(format_wait);
    format!("Claude usage limit reached. This turn is paused until the {}limit resets{}.", label.map(|label| format!("{label} ")).unwrap_or_default(), wait.map(|wait| format!(" in {wait}")).unwrap_or_default())
}
fn format_wait(wait_ms: f64) -> String {
    let total = (wait_ms / 60_000.0).ceil() as u64;
    let (hours, minutes) = (total / 60, total % 60);
    if hours == 0 { format!("{total}m") } else if minutes == 0 { format!("{hours}h") } else { format!("{hours}h {minutes}m") }
}

/// `claudeResetCreditsToContract`: grants that are paused or past `ends_at` cannot be claimed and
/// do not count.
pub fn reset_credits_to_contract(block: &Value, now_ms: i64) -> Option<Value> {
    let eligible = block["eligible"].as_bool()?;
    if !eligible {
        return None;
    }
    let live: Vec<&Value> = block["grants"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|grant| decodes_as_grant(grant))
        .filter(|grant| grant["paused"] != true && grant["usable_now"] == true && grant["ends_at"].as_str().map_or(grant["ends_at"].is_null() || grant.get("ends_at").is_none(), |ends| is_future_timestamp(ends, now_ms)))
        .collect();
    let next_id = block["next_grant_id"].as_str();
    let next = live.iter().find(|grant| next_id == grant["id"].as_str());
    let mut credits = json!({"availableCount":if next.is_some() { live.iter().map(|grant| grant["resets_left"].as_u64().unwrap_or(0)).sum::<u64>() } else { 0 }});
    if let Some(next) = next {
        if let Some(expires) = next["ends_at"].as_str().and_then(|ends| DateTime::parse_from_rfc3339(ends).ok()) {
            credits["nextExpiresAt"] = json!(expires.with_timezone(&Utc).to_rfc3339_opts(SecondsFormat::Millis, true));
        }
        credits["nextCreditId"] = next["id"].clone();
    }
    Some(credits)
}
fn is_grant_id(id: &str) -> bool {
    (1..=40).contains(&id.len()) && id.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-')
}
/// The `Grant` schema: a valid id, a non-negative integer count, and well-typed optional fields.
fn decodes_as_grant(grant: &Value) -> bool {
    grant["id"].as_str().is_some_and(is_grant_id)
        && grant["resets_left"].as_u64().is_some()
        && (grant.get("ends_at").is_none() || grant["ends_at"].is_null() || grant["ends_at"].is_string())
        && ["paused", "usable_now"].iter().all(|key| grant.get(*key).is_none() || grant[*key].is_boolean())
}
/// Rejects unparseable and calendar-invalid timestamps such as February 30.
fn is_future_timestamp(value: &str, now_ms: i64) -> bool {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let pattern = PATTERN.get_or_init(|| regex::Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})$").unwrap());
    if !pattern.is_match(value) {
        return false;
    }
    let (Ok(year), Ok(month), Ok(day)) = (value[0..4].parse::<i32>(), value[5..7].parse::<u32>(), value[8..10].parse::<u32>()) else { return false };
    chrono::NaiveDate::from_ymd_opt(year, month, day).is_some() && DateTime::parse_from_rfc3339(value).is_ok_and(|time| time.timestamp_millis() > now_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapping_merging_and_credits_match_the_unchanged_source_functions() {
        let rows: Vec<Value> = serde_json::from_str(include_str!("../tests/fixtures/claude-usage-limits.json")).unwrap();
        let mut seen = std::collections::HashMap::<String, usize>::new();
        for row in &rows {
            let operation = row["operation"].as_str().unwrap();
            let got = match operation {
                "event" => rate_limit_event_to_update(&row["info"], &ScopedLimitNames { overage_included: row["names"]["overageIncluded"].as_str().map(str::to_owned) }).unwrap_or(Value::Null),
                "response" => {
                    let (limits, names) = usage_response_to_limits(&row["response"], "2026-10-09T12:00:00.000Z");
                    assert_eq!(json!(names.overage_included), row["names"]["overageIncluded"], "{row}");
                    limits
                }
                "apply" => apply_usage_limits_update(Some(&row["previous"]).filter(|value| !value.is_null()), &row["update"], row["checkedAt"].as_str().unwrap()).unwrap_or(Value::Null),
                "resolve" => resolve_usage_limits_after_probe(Some(&row["published"]).filter(|value| !value.is_null()), Some(&row["probed"]).filter(|value| !value.is_null())).unwrap_or(Value::Null),
                "credits" => reset_credits_to_contract(&row["block"], row["nowMs"].as_i64().unwrap()).unwrap_or(Value::Null),
                other => panic!("{other}"),
            };
            assert_eq!(got, row["output"], "{row}");
            *seen.entry(operation.to_owned()).or_default() += 1;
        }
        assert!(seen.values().all(|count| *count >= 4), "{seen:?}");
    }

    #[test]
    fn a_usage_limit_notice_states_the_remaining_wait() {
        let names = ScopedLimitNames { overage_included: Some("Fable".into()) };
        let now = 1_800_000_000_000.0;
        assert_eq!(describe_usage_limit(&json!({"rateLimitType":"five_hour","resetsAt":1_800_005_400}), now, &names), "Claude usage limit reached. This turn is paused until the 5-hour limit resets in 1h 30m.");
        assert_eq!(describe_usage_limit(&json!({"rateLimitType":"seven_day_overage_included","resetsAt":1_800_000_030}), now, &names), "Claude usage limit reached. This turn is paused until the 7-day Fable limit resets in 1m.");
        assert_eq!(describe_usage_limit(&json!({"rateLimitType":"seven_day","resetsAt":1_800_007_200}), now, &names), "Claude usage limit reached. This turn is paused until the 7-day limit resets in 2h.");
        // An unknown window, a past reset, and one beyond a month ship without a wait.
        assert_eq!(describe_usage_limit(&json!({}), now, &names), "Claude usage limit reached. This turn is paused until the limit resets.");
        assert_eq!(describe_usage_limit(&json!({"rateLimitType":"overage","resetsAt":1}), now, &names), "Claude usage limit reached. This turn is paused until the overage limit resets.");
        assert_eq!(describe_usage_limit(&json!({"rateLimitType":"overage","resetsAt":1_900_000_000}), now, &names), "Claude usage limit reached. This turn is paused until the overage limit resets.");
    }
}
