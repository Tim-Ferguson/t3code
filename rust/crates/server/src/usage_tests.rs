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
        provider: match value["provider"].as_str() { Some("codex") => "codex", Some("grok") => "grok", _ => "claude" },
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

// ---- the persisted scan cache against the unchanged source (`tools/generate_usage_scan_cache_fixtures.mjs`) ------------------

mod scan_cache {
    use super::{canon, record_from, record_json};
    use crate::{
        usage_scan_cache::{CachedFile, ScanCache, ScanCacheWriter, codex_state_json, decode_codex_state, decode_scan_cache, prune_scan_cache},
        usage_transcript_reader::{ParsePosition, read_transcript_records},
    };
    use serde_json::{Map, Value, json};
    use std::sync::Arc;

    fn position_json(position: &ParsePosition) -> Value {
        json!({"resumeOffset":position.resume_offset,"guardLength":position.guard_length,"guardHash":position.guard_hash,"codexState":codex_state_json(&position.codex_state)})
    }
    fn position_from(value: &Value) -> ParsePosition {
        ParsePosition { resume_offset: value["resumeOffset"].as_u64().unwrap(), guard_length: value["guardLength"].as_u64().unwrap(), guard_hash: value["guardHash"].as_u64().unwrap() as u32, codex_state: decode_codex_state(&value["codexState"]).unwrap() }
    }
    fn provider(value: &Value) -> &'static str {
        match value.as_str().unwrap() {
            "codex" => "codex",
            "grok" => "grok",
            _ => "claude",
        }
    }
    fn entry_from(value: &Value) -> (String, Arc<CachedFile>) {
        let records = |name: &str| value[name].as_array().unwrap().iter().map(record_from).collect();
        (value["path"].as_str().unwrap().to_owned(), Arc::new(CachedFile::new(value["size"].as_i64().unwrap(), value["mtimeMs"].as_f64().unwrap(), provider(&value["provider"]), records("records"), records("tailRecords"), position_from(&value["position"]))))
    }
    fn entry_json(path: &str, entry: &CachedFile) -> Value {
        json!({"path":path,"size":entry.size,"mtimeMs":entry.mtime_ms,"provider":entry.provider,"records":entry.records.iter().map(record_json).collect::<Vec<_>>(),"tailRecords":entry.tail_records.iter().map(record_json).collect::<Vec<_>>(),"position":position_json(&entry.position)})
    }
    fn cache_from(entries: &[Value]) -> ScanCache {
        entries.iter().map(entry_from).collect()
    }
    fn view(parsed: Option<crate::usage_transcript_reader::ParseResult>) -> Value {
        match parsed {
            None => Value::Null,
            Some(parsed) => json!({"records":parsed.records.iter().map(record_json).collect::<Vec<_>>(),"tailRecords":parsed.tail_records.iter().map(record_json).collect::<Vec<_>>(),"position":position_json(&parsed.position),"resumed":parsed.resumed}),
        }
    }

    #[test]
    fn the_reader_the_document_and_the_pruning_match_the_unchanged_source() {
        let directory = tempfile::tempdir().unwrap();
        let mut seen = std::collections::HashMap::<String, usize>::new();
        for line in include_str!("../tests/fixtures/usage-scan-cache.jsonl").lines() {
            let row: Value = serde_json::from_str(line).unwrap();
            let kind = row["kind"].as_str().unwrap().to_owned();
            *seen.entry(kind.clone()).or_default() += 1;
            let name = row["name"].as_str().unwrap();
            match kind.as_str() {
                "parse" => {
                    let provider = row["provider"].as_str().unwrap();
                    let path = directory.path().join("transcript.jsonl");
                    for (index, version) in row["versions"].as_array().unwrap().iter().enumerate() {
                        std::fs::write(&path, version["text"].as_str().unwrap()).unwrap();
                        assert_eq!(canon(&view(read_transcript_records(&path, provider, None))), canon(&version["full"]), "{name}: version {index}, whole");
                        if !version["resumeFrom"].is_null() {
                            let from = position_from(&version["resumeFrom"]);
                            assert_eq!(canon(&view(read_transcript_records(&path, provider, Some(&from)))), canon(&version["resumedParse"]), "{name}: version {index}, resumed");
                        }
                    }
                }
                "write" => {
                    let extra: Map<String, Value> = row["extra"].as_object().unwrap().clone();
                    let mut writer = ScanCacheWriter::default();
                    assert_eq!(writer.write(&cache_from(row["entries"].as_array().unwrap()), &extra), row["first"].as_str().unwrap(), "{name}: first");
                    // The entries written once keep their text; the replaced and added ones are encoded on top of the same tables.
                    let mut cache = cache_from(row["entries"].as_array().unwrap());
                    let mut writer = ScanCacheWriter::default();
                    writer.write(&cache, &extra);
                    let replaced = row["replaced"].as_array().unwrap();
                    for entry in replaced {
                        let (path, entry) = entry_from(entry);
                        if cache.get(&path).is_none_or(|current| current.records.len() != entry.records.len() || current.size != entry.size) {
                            cache.insert(path, entry);
                        }
                    }
                    assert_eq!(writer.write(&cache, &extra), row["second"].as_str().unwrap(), "{name}: second");
                    // And what was written reads back as what was written.
                    let reread = decode_scan_cache(&serde_json::from_str(row["second"].as_str().unwrap()).unwrap());
                    assert_eq!(canon(&json!(reread.iter().map(|(path, entry)| entry_json(path, entry)).collect::<Vec<_>>())), canon(&row["replaced"]), "{name}: round trip");
                }
                "decode" => {
                    let got: Vec<Value> = decode_scan_cache(&row["document"]).iter().map(|(path, entry)| entry_json(path, entry)).collect();
                    assert_eq!(canon(&json!(got)), canon(&row["decoded"]), "{name}");
                }
                "prune" => {
                    let mut cache = cache_from(row["entries"].as_array().unwrap());
                    assert_eq!(prune_scan_cache(&mut cache, row["cutoff"].as_f64().unwrap()) as u64, row["removed"].as_u64().unwrap(), "{name}");
                    assert_eq!(json!(cache.keys().collect::<Vec<_>>()), row["kept"], "{name}");
                }
                other => panic!("{other}"),
            }
        }
        assert_eq!(seen.len(), 4, "{seen:?}");
    }
}
