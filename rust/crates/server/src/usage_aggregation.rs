//! Folds parsed transcript records into `(day, hourStart?, provider, model, sourcePath?)` buckets
//! (`usageAggregation.ts`). Pure: the bucketing and de-duplication rules are testable without the
//! filesystem or the network, and nothing here reads the clock.
use crate::{
    usage_pricing::{CategoryCost, CostSource, Priced, RateTable, cache_savings_usd, price_usage},
    usage_transcripts::{Speed, Totals, UsageRecord},
};
use chrono::{DateTime, FixedOffset, TimeZone, Utc};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

const HOUR_MS: i64 = 60 * 60 * 1000;

/// The zone a client buckets days in: an IANA name, or a fixed offset such as `+05:30`. An unknown
/// zone degrades to UTC rather than failing the whole scan.
#[derive(Clone, Copy)]
enum Zone {
    Named(chrono_tz::Tz),
    Fixed(FixedOffset),
}
impl Zone {
    fn parse(name: &str) -> Self {
        if let Ok(zone) = name.parse::<chrono_tz::Tz>() {
            return Zone::Named(zone);
        }
        let digits: String = name.chars().filter(|character| character.is_ascii_digit()).collect();
        let sign = match name.chars().next() {
            Some('+') => 1,
            Some('-') => -1,
            _ => return Zone::Named(chrono_tz::UTC),
        };
        let (hours, minutes) = match digits.len() {
            2 => (digits.parse::<i32>().ok(), Some(0)),
            4 => (digits[..2].parse::<i32>().ok(), digits[2..].parse::<i32>().ok()),
            _ => (None, None),
        };
        match (hours, minutes) {
            (Some(hours), Some(minutes)) if name.len() <= 6 => FixedOffset::east_opt(sign * (hours * 3600 + minutes * 60)).map_or(Zone::Named(chrono_tz::UTC), Zone::Fixed),
            _ => Zone::Named(chrono_tz::UTC),
        }
    }
    fn day(&self, timestamp_ms: i64) -> String {
        let Some(instant) = DateTime::<Utc>::from_timestamp_millis(timestamp_ms) else { return String::new() };
        match self {
            Zone::Named(zone) => zone.from_utc_datetime(&instant.naive_utc()).format("%Y-%m-%d").to_string(),
            Zone::Fixed(offset) => instant.with_timezone(offset).format("%Y-%m-%d").to_string(),
        }
    }
}

#[derive(Default)]
struct Bucket {
    totals: Totals,
    cost_usd: f64,
    cache_savings_usd: f64,
    category_cost_usd: Option<CategoryCost>,
    fast_cost_usd: f64,
    ultrafast_cost_usd: f64,
    speed_premium_usd: f64,
    records: u64,
    unpriced_records: u64,
    provider_reported_records: u64,
    sessions: HashSet<String>,
}

pub struct Options {
    pub time_zone: String,
    pub since_day: String,
    pub until_day: String,
    pub rates: RateTable,
    pub price_overrides: Option<RateTable>,
    /// From [`resolve_model_aliases`]. Mapped records bucket and price as their target.
    pub model_aliases: HashMap<String, String>,
    /// An exact `[since, until)` hourly window in epoch milliseconds, `None` for daily buckets.
    pub hourly: Option<(i64, i64)>,
}

/// `resolveModelAliases`: user model mappings resolve to their final target, so `a -> b` and
/// `b -> c` both land on `c`. A model whose chain enters a loop is left unmapped.
pub fn resolve_model_aliases(aliases: &[(String, String)]) -> HashMap<String, String> {
    let lookup: HashMap<&str, &str> = aliases.iter().map(|(from, to)| (from.as_str(), to.as_str())).collect();
    let mut resolved = HashMap::new();
    for (model, _) in aliases {
        let mut seen = HashSet::from([model.as_str()]);
        let mut target = lookup[model.as_str()];
        while lookup.contains_key(target) && !seen.contains(target) {
            seen.insert(target);
            target = lookup[target];
        }
        // Stopping on a mapped model means the chain entered a loop.
        if !lookup.contains_key(target) {
            resolved.insert(model.clone(), target.to_owned());
        }
    }
    resolved
}

pub struct Aggregator {
    options: Options,
    zone: Zone,
    keys: Vec<(String, String, &'static str, String, String)>,
    buckets: HashMap<(String, String, &'static str, String, String), usize>,
    storage: Vec<Bucket>,
    seen: HashSet<String>,
    duplicates_dropped: u64,
    out_of_window: u64,
}

pub struct Aggregated {
    pub buckets: Vec<Value>,
    /// Records dropped because an earlier record carried the same dedupe key.
    pub duplicates_dropped: u64,
    /// Records whose day fell outside the requested window.
    pub out_of_window: u64,
}

impl Aggregator {
    pub fn new(options: Options) -> Self {
        let zone = Zone::parse(&options.time_zone);
        Self { options, zone, keys: vec![], buckets: HashMap::new(), storage: vec![], seen: HashSet::new(), duplicates_dropped: 0, out_of_window: 0 }
    }

    /// Folds one record in. Returns whether it actually contributed, so callers can derive
    /// per-window facts (distinct sessions) from the records that landed.
    pub fn add(&mut self, input: &UsageRecord, source_path: &str) -> bool {
        let model = self.options.model_aliases.get(&input.model).cloned().unwrap_or_else(|| input.model.clone());
        if let Some(key) = &input.dedupe_key {
            if !self.seen.insert(key.clone()) {
                self.duplicates_dropped += 1;
                return false;
            }
        }
        if let Some((since, until)) = self.options.hourly {
            if input.timestamp_ms < since || input.timestamp_ms >= until {
                self.out_of_window += 1;
                return false;
            }
        }
        let day = self.zone.day(input.timestamp_ms);
        if self.options.hourly.is_none() && (day < self.options.since_day || day > self.options.until_day) {
            self.out_of_window += 1;
            return false;
        }
        let hour_start = match self.options.hourly {
            None => String::new(),
            Some((since, _)) => {
                let index = (input.timestamp_ms - since).div_euclid(HOUR_MS);
                DateTime::<Utc>::from_timestamp_millis(since + index * HOUR_MS).map(|time| time.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)).unwrap_or_default()
            }
        };
        let key = (day, hour_start, input.provider, model.clone(), source_path.to_owned());
        let index = match self.buckets.get(&key) {
            Some(index) => *index,
            None => {
                self.storage.push(Bucket::default());
                self.keys.push(key.clone());
                self.buckets.insert(key, self.storage.len() - 1);
                self.storage.len() - 1
            }
        };
        let priced_record = Priced { model: &model, totals: &input.totals, speed: input.speed, reported_cost_usd: input.reported_cost_usd };
        let priced = price_usage(&self.options.rates, &priced_record, self.options.price_overrides.as_ref());
        let savings = cache_savings_usd(&self.options.rates, &priced_record, self.options.price_overrides.as_ref());
        let bucket = &mut self.storage[index];
        bucket.totals.uncached_input += input.totals.uncached_input;
        bucket.totals.cached_input += input.totals.cached_input;
        bucket.totals.cache_creation += input.totals.cache_creation;
        bucket.totals.output += input.totals.output;
        bucket.totals.reasoning += input.totals.reasoning;
        bucket.cost_usd += priced.cost_usd;
        if let Some(add) = priced.category_cost_usd {
            bucket.category_cost_usd = Some(match bucket.category_cost_usd {
                None => add,
                Some(sum) => CategoryCost { input: sum.input + add.input, cache_read: sum.cache_read + add.cache_read, cache_write: sum.cache_write + add.cache_write, output: sum.output + add.output },
            });
        }
        if input.speed == Speed::Fast {
            bucket.fast_cost_usd += priced.cost_usd;
        }
        if input.speed == Speed::Ultrafast {
            bucket.ultrafast_cost_usd += priced.cost_usd;
        }
        bucket.speed_premium_usd += priced.speed_premium_usd;
        bucket.cache_savings_usd += savings;
        bucket.records += 1;
        if priced.cost_source == CostSource::Unpriced {
            bucket.unpriced_records += 1;
        }
        if priced.cost_source == CostSource::ProviderReported {
            bucket.provider_reported_records += 1;
        }
        if !input.session_id.is_empty() {
            bucket.sessions.insert(input.session_id.clone());
        }
        true
    }

    pub fn finish(self) -> Aggregated {
        let mut rows: Vec<(usize, Value)> = vec![];
        for (index, bucket) in self.storage.iter().enumerate() {
            let (day, hour_start, provider, model, source_path) = &self.keys[index];
            let (fast, ultrafast, premium) = (round_usd(bucket.fast_cost_usd), round_usd(bucket.ultrafast_cost_usd), round_usd(bucket.speed_premium_usd));
            let mut row = json!({
                "day":day,"provider":provider,"model":model,
                "totals":{"uncachedInputTokens":bucket.totals.uncached_input,"cachedInputTokens":bucket.totals.cached_input,"cacheCreationTokens":bucket.totals.cache_creation,"outputTokens":bucket.totals.output,"reasoningTokens":bucket.totals.reasoning},
                "costUsd":bucket.cost_usd,"cacheSavingsUsd":bucket.cache_savings_usd,
                "costSource":cost_source(bucket).wire(),
                "records":bucket.records,"unpricedRecords":bucket.unpriced_records,"sessions":bucket.sessions.len(),
            });
            if !hour_start.is_empty() {
                row["hourStart"] = json!(hour_start);
            }
            if !source_path.is_empty() {
                row["sourcePath"] = json!(source_path);
            }
            // Zero and unknown figures are omitted to keep payloads small.
            if let Some(category) = bucket.category_cost_usd {
                row["categoryCostUsd"] = json!({"input":round_usd(category.input),"cacheRead":round_usd(category.cache_read),"cacheWrite":round_usd(category.cache_write),"output":round_usd(category.output)});
            }
            if fast != 0.0 {
                row["fastCostUsd"] = json!(fast);
            }
            if ultrafast != 0.0 {
                row["ultrafastCostUsd"] = json!(ultrafast);
            }
            if premium != 0.0 {
                row["speedPremiumUsd"] = json!(premium);
            }
            rows.push((index, row));
        }
        // Stable ordering keeps payloads diffable and snapshot tests meaningful.
        let collator = collator();
        let text = |row: &Value, field: &str| row[field].as_str().unwrap_or("").to_owned();
        rows.sort_by(|(_, a), (_, b)| {
            collator
                .compare(&text(a, "day"), &text(b, "day"))
                .then_with(|| collator.compare(&text(a, "hourStart"), &text(b, "hourStart")))
                .then_with(|| collator.compare(&text(a, "provider"), &text(b, "provider")))
                .then_with(|| collator.compare(&text(a, "model"), &text(b, "model")))
        });
        Aggregated { buckets: rows.into_iter().map(|(_, row)| row).collect(), duplicates_dropped: self.duplicates_dropped, out_of_window: self.out_of_window }
    }
}

fn collator() -> icu_collator::CollatorBorrowed<'static> {
    let preferences: icu_collator::CollatorPreferences = crate::workspace_entries::locale_from_environment().into();
    icu_collator::Collator::try_new(preferences, Default::default()).expect("compiled ICU collation data")
}

/// Rounds to micro-dollars: the split and speed figures need no more precision.
fn round_usd(value: f64) -> f64 {
    (value * 1e6).round() / 1e6
}

/// A bucket mixes records from one model, but their cost provenance can differ when only some
/// carried a reported cost. The weakest provenance wins so the UI never overstates confidence.
fn cost_source(bucket: &Bucket) -> CostSource {
    if bucket.unpriced_records == bucket.records {
        CostSource::Unpriced
    } else if bucket.provider_reported_records == bucket.records {
        CostSource::ProviderReported
    } else {
        CostSource::ModelPriced
    }
}
