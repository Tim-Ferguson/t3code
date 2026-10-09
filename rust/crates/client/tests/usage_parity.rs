//! The Usage page's logic replayed against the original (`rust/tools/generate_client_usage_fixtures.mjs`):
//! number, date and window formats in several zones, merging of environments' summaries, the
//! breakdown rules, the chart's scale and curve, and the limits' accounts, pools, pace and wording.
use serde_json::{Value, json};
use std::collections::HashMap;
use t3_client::usage::{self, EnvironmentUsage, Zone};
use t3_client::usage_limits::{self as limits, Presentation};

fn rows(kind: &str) -> Vec<Value> {
    let rows: Vec<Value> = include_str!("fixtures/usage.jsonl").lines().map(|line| serde_json::from_str(line).unwrap()).filter(|row: &Value| row["type"] == kind).collect();
    assert!(!rows.is_empty(), "no rows for {kind}");
    rows
}
/// Every number as a float, so `2` and `2.0` compare equal.
fn normal(value: &Value) -> Value {
    match value {
        Value::Number(number) => json!(number.as_f64().unwrap()),
        Value::Array(items) => Value::Array(items.iter().map(normal).collect()),
        Value::Object(fields) => Value::Object(fields.iter().map(|(key, value)| (key.clone(), normal(value))).collect()),
        other => other.clone(),
    }
}
fn same(actual: &impl serde::Serialize, expected: &Value, context: &str) {
    let actual = normal(&serde_json::to_value(actual).unwrap());
    assert_eq!(actual, normal(expected), "{context}");
}

/// The offset rules the fixture zones follow: fixed, the United States', and the European Union's.
fn offset(rule: &Value, ms: i64) -> i32 {
    use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Utc, Weekday};
    let nth_sunday = |year: i32, month: u32, nth: u32| {
        let first = NaiveDate::from_ymd_opt(year, month, 1).unwrap();
        let shift = (7 + Weekday::Sun.num_days_from_monday() as i64 - first.weekday().num_days_from_monday() as i64) % 7;
        first + Duration::days(shift + 7 * (nth as i64 - 1))
    };
    let last_sunday = |year: i32, month: u32| {
        let last = NaiveDate::from_ymd_opt(year, month + 1, 1).unwrap() - Duration::days(1);
        last - Duration::days(((last.weekday().num_days_from_monday() + 1) % 7) as i64)
    };
    let at = DateTime::from_timestamp_millis(ms).unwrap();
    if let Some(fixed) = rule["fixed"].as_i64() {
        return fixed as i32;
    }
    if let Some(standard) = rule["us"].as_i64() {
        let year = at.year();
        let start = Utc.from_utc_datetime(&nth_sunday(year, 3, 2).and_hms_opt(2, 0, 0).unwrap()) - Duration::minutes(standard);
        let end = Utc.from_utc_datetime(&nth_sunday(year, 11, 1).and_hms_opt(2, 0, 0).unwrap()) - Duration::minutes(standard + 60);
        return (standard + if at >= start && at < end { 60 } else { 0 }) as i32;
    }
    let standard = rule["eu"].as_i64().unwrap();
    let year = at.year();
    let start = Utc.from_utc_datetime(&last_sunday(year, 3).and_hms_opt(1, 0, 0).unwrap());
    let end = Utc.from_utc_datetime(&last_sunday(year, 10).and_hms_opt(1, 0, 0).unwrap());
    (standard + if at >= start && at < end { 60 } else { 0 }) as i32
}

#[test]
fn numbers_and_percentages_read_as_the_original_prints_them() {
    for row in rows("number") {
        let value = row["value"].as_f64().unwrap();
        assert_eq!(usage::format_usd(value), row["usd"].as_str().unwrap(), "dollars of {value}");
        assert_eq!(usage::format_count(value), row["count"].as_str().unwrap(), "count of {value}");
        assert_eq!(usage::format_tokens(value), row["tokens"].as_str().unwrap(), "tokens of {value}");
    }
    for row in rows("percent") {
        let share = row["share"].as_f64().unwrap();
        assert_eq!(usage::format_percent(share, 1), row["one"].as_str().unwrap(), "{share}");
        assert_eq!(usage::format_percent(share, 0), row["zero"].as_str().unwrap(), "{share}");
        assert_eq!(usage::format_percent(share, 2), row["two"].as_str().unwrap(), "{share}");
    }
    for row in rows("mismatch") {
        assert_eq!(usage::format_contract_mismatch(row["label"].as_str().unwrap(), row["direction"].as_str().unwrap()), row["output"].as_str().unwrap());
    }
}

#[test]
fn days_and_hours_of_a_window_are_listed_and_named_as_the_original_does() {
    for row in rows("dayShort") {
        assert_eq!(usage::format_day_short(row["day"].as_str().unwrap()), row["output"].as_str().unwrap(), "{}", row["day"]);
    }
    for row in rows("days") {
        let expected: Vec<String> = serde_json::from_value(row["output"].clone()).unwrap();
        assert_eq!(usage::enumerate_days(row["since"].as_str().unwrap(), row["until"].as_str().unwrap()), expected, "{row}");
    }
    for row in rows("hours") {
        let expected: Vec<String> = serde_json::from_value(row["output"].clone()).unwrap();
        assert_eq!(usage::enumerate_hour_starts(row["since"].as_str().unwrap(), row["until"].as_str().unwrap()), expected, "{row}");
    }
}

#[test]
fn instants_are_worded_in_the_viewers_zone_including_the_repeated_hour() {
    for row in rows("instant") {
        let rule = row["rule"].clone();
        let function = move |ms: i64| offset(&rule, ms);
        let zone = Zone { name: row["zone"].as_str().unwrap(), offset: &function };
        let instant = row["instant"].as_str().unwrap();
        let context = format!("{} {instant}", row["zone"]);
        assert_eq!(usage::format_hour_short(instant, zone), row["hour"].as_str().unwrap(), "hour {context}");
        assert_eq!(usage::format_date_time_short(instant, zone), row["dateTime"].as_str().unwrap(), "date and time {context}");
        let references = ["2026-11-01T18:00:00Z", "2026-11-02T18:00:00Z", "2026-11-04T18:00:00Z"];
        for (reference, expected) in references.iter().zip(row["relative"].as_array().unwrap()) {
            assert_eq!(usage::format_relative_hour_short(instant, reference, zone), expected.as_str().unwrap(), "relative to {reference} {context}");
        }
    }
}

#[test]
fn a_window_is_the_viewers_calendar_days_or_an_exact_rolling_day() {
    for row in rows("window") {
        let rule = row["rule"].clone();
        let function = move |ms: i64| offset(&rule, ms);
        let name = row["output"]["timeZone"].as_str().unwrap();
        let zone = Zone { name, offset: &function };
        let now = chrono::DateTime::parse_from_rfc3339(row["now"].as_str().unwrap()).unwrap().timestamp_millis();
        let window = usage::make_window(row["days"].as_i64().unwrap(), now, zone, row["resolution"] == "hour");
        assert_eq!(normal(&window.to_value()), normal(&row["output"]), "{} {} {}", row["zone"], row["now"], row["days"]);
    }
}

#[test]
fn environments_merge_into_one_view_without_counting_a_directory_twice() {
    for row in rows("merge") {
        let environments: Vec<EnvironmentUsage> = serde_json::from_value(row["environments"].clone()).unwrap();
        let merged = usage::merge_usage(&environments, usage::USAGE_CONTRACT_VERSION);
        same(&merged, &row["expected"], row["name"].as_str().unwrap());
        let unknown: Vec<bool> = merged.models.iter().map(|model| model.cost_unknown()).collect();
        let expected: Vec<bool> = serde_json::from_value(row["unknown"].clone()).unwrap();
        assert_eq!(unknown, expected, "unknown costs of {}", row["name"]);
    }
    for row in rows("compatible") {
        for check in row["checks"].as_array().unwrap() {
            assert_eq!(usage::is_compatible_contract_version(check[0].as_f64().unwrap(), usage::USAGE_CONTRACT_VERSION), check[1] == true, "{check}");
        }
    }
}

#[test]
fn the_breakdown_ranks_and_splits_as_the_original_does() {
    let row = rows("breakdown").remove(0);
    let models: Vec<usage::ModelTotals> = row["models"].as_array().unwrap().iter().map(|model| {
        let tokens: usage::TokenTotals = serde_json::from_value(model["tokens"].clone()).unwrap();
        usage::ModelTotals {
            model: model["model"].as_str().unwrap().into(),
            provider: model["provider"].as_str().unwrap().into(),
            cost_usd: model["costUsd"].as_f64().unwrap(),
            total_tokens: model["totalTokens"].as_f64().unwrap(),
            tokens,
            records: model["records"].as_f64().unwrap(),
            unpriced_records: model["unpricedRecords"].as_f64().unwrap(),
            unpriced_tokens: model["unpricedTokens"].as_f64().unwrap(),
            cost_share: model["costShare"].as_f64().unwrap(),
            token_share: model["tokenShare"].as_f64().unwrap(),
        }
    }).collect();
    let by_tokens: Vec<String> = usage::sort_models_by_tokens(&models).into_iter().map(|model| model.model).collect();
    assert_eq!(json!(by_tokens), row["byTokens"]);
    let shares: Vec<Value> = models.iter().map(|model| json!([usage::model_share(model, false), usage::model_share(model, true)])).collect();
    same(&shares, &row["shares"], "shares");
    let hits: Vec<Value> = models.iter().map(|model| json!(usage::cache_hit_rate(model))).collect();
    same(&hits, &row["hit"], "cache hit rates");
    let per_million: Vec<Value> = models.iter().map(|model| json!(usage::cost_per_million_tokens(model))).collect();
    same(&per_million, &row["perMillion"], "cost per million tokens");
    // The segments come from the merged scenario's totals.
    let environments: Vec<EnvironmentUsage> = serde_json::from_value(rows("merge")[1]["environments"].clone()).unwrap();
    let merged = usage::merge_usage(&environments, usage::USAGE_CONTRACT_VERSION);
    same(&usage::cost_type_segments(&merged.category_cost), &row["costSegments"], "cost segments");
    let tokens = usage::TokenTotals { uncached_input_tokens: merged.uncached_input_tokens, cached_input_tokens: merged.cached_input_tokens, cache_creation_tokens: merged.cache_creation_tokens, output_tokens: merged.output_tokens, reasoning_tokens: 0.0 };
    same(&usage::token_type_segments(&tokens), &row["tokenSegments"], "token segments");
    same(&usage::speed_cost_segments(&merged.speed_cost), &row["speedSegments"], "speed segments");
    assert_eq!(json!(usage::providers_with_usage(&merged.providers)), row["withUsage"]);
}

#[test]
fn the_chart_columns_scale_and_curve_are_the_originals() {
    for row in rows("columns") {
        let tokens = row["metric"] == "tokens";
        let periods: Vec<String> = serde_json::from_value(row["periods"].clone()).unwrap();
        let (key, field) = if row["resolution"] == "hour" { ("hourStart", "hourly") } else { ("day", "daily") };
        let entries: Vec<(String, indexmap::IndexMap<String, usage::Share>)> = row[field].as_array().unwrap().iter().map(|entry| (entry[key].as_str().unwrap().to_owned(), serde_json::from_value::<indexmap::IndexMap<String, ShareRow>>(entry["byProvider"].clone()).unwrap().into_iter().map(|(provider, share)| (provider, usage::Share { cost_usd: share.cost_usd, total_tokens: share.total_tokens })).collect())).collect();
        let by_period: HashMap<&str, &indexmap::IndexMap<String, usage::Share>> = entries.iter().map(|(period, providers)| (period.as_str(), providers)).collect();
        same(&usage::build_period_columns(&periods, &by_period, tokens), &row["expected"], &format!("{} {}", row["resolution"], row["metric"]));
    }
    for row in rows("scale") {
        let (max, ticks) = usage::nice_scale(row["peak"].as_f64().unwrap(), usage::TICK_COUNT);
        same(&json!({"max":max,"ticks":ticks}), &row["expected"], &format!("{}", row["peak"]));
    }
    for row in rows("curve") {
        let points: Vec<usage::Point> = row["points"].as_array().unwrap().iter().map(|point| usage::Point { x: point["x"].as_f64().unwrap(), y: point["y"].as_f64().unwrap() }).collect();
        assert_eq!(usage::curve_path(&points), row["path"].as_str().unwrap(), "{}", row["points"]);
    }
}
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShareRow {
    cost_usd: f64,
    total_tokens: f64,
}

fn presentations(row: &Value) -> Vec<Presentation> {
    row["environments"].as_array().unwrap().iter().map(|environment| Presentation {
        environment_id: environment["id"].as_str().unwrap().into(),
        label: environment["label"].as_str().unwrap().into(),
        providers: limits::providers_from(&environment["providers"]),
        sources: limits::sources_from(&environment["usageLimitSources"]),
    }).collect()
}

#[test]
fn accounts_pool_by_provider_and_window_as_the_original_pools_them() {
    for row in rows("limits") {
        let name = format!("{} ({} environments)", row["name"], row["environments"].as_array().unwrap().len());
        let presentations = presentations(&row);
        let now = row["now"].as_f64().unwrap();
        let accounts = limits::collect_accounts(&presentations);
        same(&accounts, &row["accounts"], &format!("accounts of {name}"));
        let pools = limits::collect_pools(&accounts, now);
        same(&pools, &row["pools"], &format!("pools of {name}"));
        let displayed: Vec<Vec<String>> = pools.iter().map(|pool| limits::display_windows(pool).into_iter().map(|window| format!("{}:{}", window.kind, window.id)).collect()).collect();
        assert_eq!(json!(displayed), row["displayed"], "displayed windows of {name}");
        assert_eq!(json!(limits::collect_notices(&presentations)), row["notices"], "notices of {name}");
        same(&limits::collect_external_usage_links(&presentations), &row["links"], &format!("links of {name}"));
        let first = &presentations[0];
        let with: Vec<&str> = limits::providers_with_limits(&first.providers).into_iter().map(|provider| provider.instance_id.as_str()).collect();
        assert_eq!(json!(with), row["withLimits"], "providers with limits in {name}");
        let has: Vec<Value> = row["hasLimits"].as_array().unwrap().iter().map(|entry| json!([entry[0], limits::has_provider_limits(entry[0].as_str().unwrap(), &first.providers, &first.sources)])).collect();
        assert_eq!(json!(has), row["hasLimits"], "coverage of {name}");
        if presentations.len() == 1 {
            for entry in row["reports"].as_array().unwrap() {
                let report = limits::collect_provider_report(entry[0].as_str().unwrap(), &first.providers, &first.sources, now);
                same(&report, &entry[1], &format!("report of {} in {name}", entry[0]));
            }
        }
        let sharing: Vec<bool> = first.providers.iter().map(limits::uses_chatgpt_sharing).collect();
        assert_eq!(json!(sharing), row["sharing"], "sharing in {name}");
    }
}

#[test]
fn a_window_reads_its_remaining_quota_pace_and_reset_as_the_original_words_them() {
    for row in rows("window-math") {
        let window: limits::Window = serde_json::from_value(row["window"].clone()).unwrap();
        let now = row["now"].as_f64().unwrap();
        let context = row["window"].to_string();
        assert_eq!(limits::remaining_percent(&window), row["remaining"].as_f64().unwrap(), "{context}");
        same(&limits::elapsed_share(&window, now), &row["elapsed"], &context);
        assert_eq!(limits::pace_of(&window, now), row["pace"].as_str(), "{context}");
        assert_eq!(limits::format_resets_in(&window, now).as_deref(), row["resetsIn"].as_str(), "{context}");
    }
    for row in rows("duration") {
        assert_eq!(limits::format_duration(row["ms"].as_f64().unwrap()), row["output"].as_str().unwrap(), "{}", row["ms"]);
    }
    for row in rows("command") {
        assert_eq!(limits::is_usage_limits_command(row["prompt"].as_str().unwrap()), row["output"] == true, "{}", row["prompt"]);
    }
    for row in rows("coverage") {
        let previous = limits::sources_from(&sources(&row["previous"]));
        let next = limits::sources_from(&sources(&row["next"]));
        assert_eq!(limits::same_command_coverage(&previous, &next), row["output"] == true, "{row}");
    }
}
/// The coverage fixtures name only an account's driver; the decoder wants whole accounts.
fn sources(value: &Value) -> Value {
    Value::Array(value.as_array().unwrap().iter().enumerate().map(|(index, source)| json!({
        "id": format!("s{index}"), "label": "S", "error": source["error"],
        "accounts": source["accounts"].as_array().unwrap().iter().map(|account| json!({"id":"a","driver":account["driver"],"usageLimits":{"checkedAt":"2026-10-09T00:00:00.000Z","windows":[]}})).collect::<Vec<_>>(),
    })).collect())
}

#[test]
fn the_run_a_usage_limit_stopped_is_found_as_the_original_finds_it() {
    for row in rows("blocked") {
        let runs: Vec<Value> = serde_json::from_value(row["runs"].clone()).unwrap();
        let items: Vec<Value> = serde_json::from_value(row["items"].clone()).unwrap();
        let blocked = limits::blocked_run(&runs, &items);
        assert_eq!(blocked.as_ref().map(|blocked| blocked.run_id.as_str()), row["run"].as_str(), "{}", row["name"]);
        assert_eq!(blocked.as_ref().and_then(|blocked| blocked.reset_at.as_deref()), row["resetAt"].as_str(), "reset of {}", row["name"]);
        assert_eq!(blocked.as_ref().filter(|blocked| blocked.presented_as_latest).map(|blocked| blocked.run_id.as_str()), row["presented"].as_str(), "presented as latest in {}", row["name"]);
    }
}
