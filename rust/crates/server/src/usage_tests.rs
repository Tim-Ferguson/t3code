//! The usage parsers, rate table, pricing and aggregation against the unchanged TypeScript
//! (`tools/generate_usage_fixtures.mjs`).
use crate::{
    usage_aggregation::{Aggregator, Options, resolve_model_aliases},
    usage_pricing::{ModelRate, Priced, TokenRates, cache_savings_usd, override_table, parse_rate_table, price_usage},
    usage_transcripts::{CodexScanState, Speed, Totals, UsageRecord, parse_claude_line, parse_codex_line},
};
use serde_json::{Value, json};
use std::collections::HashMap;

/// Whole-valued floats and integers are one number, as they are after `JSON.parse`.
fn canon(value: &Value) -> Value {
    match value {
        Value::Number(number) => match number.as_f64() {
            Some(float) if float.fract() == 0.0 && float.abs() < 1e15 => json!(float as i64),
            _ => value.clone(),
        },
        Value::Array(items) => Value::Array(items.iter().map(canon).collect()),
        Value::Object(map) => Value::Object(map.iter().map(|(key, value)| (key.clone(), canon(value))).collect()),
        other => other.clone(),
    }
}
fn speed_name(speed: Speed) -> &'static str {
    match speed {
        Speed::Standard => "standard",
        Speed::Fast => "fast",
        Speed::Ultrafast => "ultrafast",
    }
}
fn totals_json(totals: &Totals) -> Value {
    json!({"uncachedInputTokens":totals.uncached_input,"cachedInputTokens":totals.cached_input,"cacheCreationTokens":totals.cache_creation,"outputTokens":totals.output,"reasoningTokens":totals.reasoning})
}
fn totals_from(value: &Value) -> Totals {
    let field = |name: &str| value[name].as_u64().unwrap_or(0);
    Totals { uncached_input: field("uncachedInputTokens"), cached_input: field("cachedInputTokens"), cache_creation: field("cacheCreationTokens"), output: field("outputTokens"), reasoning: field("reasoningTokens") }
}
fn speed_from(value: &Value) -> Speed {
    match value.as_str() {
        Some("fast") => Speed::Fast,
        Some("ultrafast") => Speed::Ultrafast,
        _ => Speed::Standard,
    }
}
fn record_json(record: &UsageRecord) -> Value {
    json!({"provider":record.provider,"timestampMs":record.timestamp_ms,"model":record.model,"sessionId":record.session_id,"totals":totals_json(&record.totals),"reportedCostUsd":record.reported_cost_usd,"speed":speed_name(record.speed),"dedupeKey":record.dedupe_key})
}
fn record_from(value: &Value) -> UsageRecord {
    UsageRecord {
        provider: if value["provider"] == "codex" { "codex" } else { "claude" },
        timestamp_ms: value["timestampMs"].as_i64().unwrap(),
        model: value["model"].as_str().unwrap().to_owned(),
        session_id: value["sessionId"].as_str().unwrap().to_owned(),
        totals: totals_from(&value["totals"]),
        reported_cost_usd: value["reportedCostUsd"].as_f64(),
        speed: speed_from(&value["speed"]),
        dedupe_key: value["dedupeKey"].as_str().map(str::to_owned),
    }
}
fn rates_json(rates: &TokenRates) -> Value {
    json!({"inputCostPerToken":rates.input,"outputCostPerToken":rates.output,"cacheReadCostPerToken":rates.cache_read,"cacheCreationCostPerToken":rates.cache_creation})
}
fn rate_json(rate: &ModelRate) -> Value {
    let mut value = rates_json(&rate.standard);
    value["fast"] = rate.fast.as_ref().map_or(Value::Null, rates_json);
    value["ultrafast"] = rate.ultrafast.as_ref().map_or(Value::Null, rates_json);
    value
}

#[test]
fn parsers_rates_pricing_and_aggregation_match_the_unchanged_source() {
    let rows: Vec<Value> = serde_json::from_str(include_str!("../tests/fixtures/usage-parity.json")).unwrap();
    let mut states: HashMap<String, CodexScanState> = HashMap::new();
    let mut table = None;
    let mut overrides = None;
    let mut seen = HashMap::<String, usize>::new();
    for row in &rows {
        let operation = row["operation"].as_str().unwrap();
        *seen.entry(operation.to_owned()).or_default() += 1;
        match operation {
            "claude" => assert_eq!(canon(&parse_claude_line(row["line"].as_str().unwrap()).map_or(Value::Null, |record| record_json(&record))), canon(&row["output"]), "{row}"),
            "codex" => {
                let state = states.entry(row["scenario"].as_str().unwrap().to_owned()).or_default();
                assert_eq!(canon(&parse_codex_line(row["line"].as_str().unwrap(), state).map_or(Value::Null, |record| record_json(&record))), canon(&row["output"]), "{row}");
            }
            "table" => {
                let parsed = parse_rate_table(&row["document"]);
                let mut got: Vec<Value> = parsed.iter().map(|(key, rate)| json!([key, rate_json(rate)])).collect();
                got.sort_by(|a, b| a[0].as_str().cmp(&b[0].as_str()));
                assert_eq!(canon(&json!(got)), canon(&row["output"]), "rate table");
                table = Some(parsed);
            }
            "overrides" => {
                let built = override_table(row["overrides"].as_object().unwrap().iter().map(|(model, prices)| (model.clone(), prices.clone())));
                let mut got: Vec<Value> = built.iter().map(|(key, rate)| json!([key, rate_json(rate)])).collect();
                got.sort_by(|a, b| a[0].as_str().cmp(&b[0].as_str()));
                let mut want = row["output"].as_array().unwrap().clone();
                want.sort_by(|a, b| a[0].as_str().cmp(&b[0].as_str()));
                assert_eq!(canon(&json!(got)), canon(&json!(want)), "overrides");
                overrides = Some(built);
            }
            "price" => {
                let totals = totals_from(&row["totals"]);
                let record = Priced { model: row["model"].as_str().unwrap(), totals: &totals, speed: speed_from(&row["speed"]), reported_cost_usd: row["reported"].as_f64() };
                let priced = price_usage(table.as_ref().unwrap(), &record, overrides.as_ref());
                let got = json!({"costUsd":priced.cost_usd,"costSource":priced.cost_source.wire(),"categoryCostUsd":priced.category_cost_usd.map(|c| json!({"input":c.input,"cacheRead":c.cache_read,"cacheWrite":c.cache_write,"output":c.output})),"speedPremiumUsd":priced.speed_premium_usd});
                assert_eq!(canon(&got), canon(&row["output"]), "{row}");
                assert_eq!(canon(&json!(cache_savings_usd(table.as_ref().unwrap(), &record, overrides.as_ref()))), canon(&row["savings"]), "{row}");
            }
            "aliases" => {
                let aliases: Vec<(String, String)> = row["aliases"].as_object().unwrap().iter().map(|(from, to)| (from.clone(), to.as_str().unwrap().to_owned())).collect();
                let mut got: Vec<Value> = resolve_model_aliases(&aliases).into_iter().map(|(from, to)| json!([from, to])).collect();
                got.sort_by(|a, b| a[0].as_str().cmp(&b[0].as_str()));
                assert_eq!(json!(got), row["output"]);
            }
            "aggregate" => {
                let options = &row["options"];
                let hourly = (options["resolution"] == "hour").then(|| (options["sinceTimeMs"].as_i64().unwrap(), options["untilTimeMs"].as_i64().unwrap()));
                let mut aggregator = Aggregator::new(Options {
                    time_zone: options["timeZone"].as_str().unwrap().to_owned(),
                    since_day: options["sinceDay"].as_str().unwrap().to_owned(),
                    until_day: options["untilDay"].as_str().unwrap().to_owned(),
                    rates: table.clone().unwrap(),
                    price_overrides: overrides.clone(),
                    model_aliases: resolve_model_aliases(&[("claude-haiku-4-5[1m]".to_owned(), "claude-haiku-4-5".to_owned())]),
                    hourly,
                });
                let source = row["sourcePath"].as_str().unwrap();
                let contributed: Vec<bool> = row["records"].as_array().unwrap().iter().map(|record| aggregator.add(&record_from(record), source)).collect();
                let finished = aggregator.finish();
                assert_eq!(json!(contributed), row["output"]["contributed"], "{options}");
                assert_eq!(canon(&json!(finished.buckets)), canon(&row["output"]["buckets"]), "{options}");
                assert_eq!((finished.duplicates_dropped, finished.out_of_window), (row["output"]["duplicatesDropped"].as_u64().unwrap(), row["output"]["outOfWindow"].as_u64().unwrap()), "{options}");
            }
            other => panic!("{other}"),
        }
    }
    assert_eq!(seen.len(), 7, "{seen:?}");
}
