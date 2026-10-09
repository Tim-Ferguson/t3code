//! The Usage page's logic: what `server.getUsageSummary` returns, merged into the view the page
//! draws, the number, date and window formats, the breakdown tables' rules and the chart's
//! scale and curve. Port of `usageMerge.ts`, `usageFormat.ts`, `usageBreakdown.ts`,
//! `usageProviders.ts` and the pure functions of `UsageProviderChart.tsx`, replayed against the
//! originals by `tests/usage_parity.rs`.
//!
//! Numbers are `f64`, as the original's are, and every sum runs in the original's order, so the
//! results are the same bits. A day or an hour is read in a zone the caller supplies as a
//! function from an instant to its offset, because only the host knows the viewer's zone.
use chrono::{DateTime, Datelike, Duration, NaiveDate, Timelike};
use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

pub const USAGE_CONTRACT_VERSION: f64 = 6.0;
pub const USAGE_MERGE_COMPATIBLE_SINCE: f64 = 4.0;

/// Unknown array entries are skipped, so a provider added later does not fail the page.
fn lenient<'de, D: Deserializer<'de>, T: DeserializeOwned>(deserializer: D) -> Result<Vec<T>, D::Error> {
    let values = Vec::<Value>::deserialize(deserializer)?;
    Ok(values.into_iter().filter_map(|value| serde_json::from_value(value).ok()).collect())
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TokenTotals {
    pub uncached_input_tokens: f64,
    pub cached_input_tokens: f64,
    pub cache_creation_tokens: f64,
    pub output_tokens: f64,
    pub reasoning_tokens: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CategoryCost {
    pub input: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub output: f64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Bucket {
    pub day: String,
    #[serde(default)]
    pub hour_start: Option<String>,
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub source_path: Option<String>,
    pub totals: TokenTotals,
    pub cost_usd: f64,
    #[serde(default)]
    pub cache_savings_usd: f64,
    #[serde(default)]
    pub category_cost_usd: Option<CategoryCost>,
    #[serde(default)]
    pub fast_cost_usd: Option<f64>,
    #[serde(default)]
    pub ultrafast_cost_usd: Option<f64>,
    #[serde(default)]
    pub speed_premium_usd: Option<f64>,
    #[serde(default)]
    pub cost_source: String,
    #[serde(default)]
    pub records: f64,
    #[serde(default)]
    pub unpriced_records: f64,
    #[serde(default)]
    pub sessions: f64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fingerprint {
    pub host_id: String,
    pub provider: String,
    pub resolved_home_path: String,
    #[serde(default)]
    pub volume_id: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Source {
    pub fingerprint: Fingerprint,
    pub status: String,
    #[serde(default)]
    pub scanned_files: f64,
    #[serde(default)]
    pub skipped_files: f64,
    #[serde(default)]
    pub malformed_records: f64,
    #[serde(default)]
    pub distinct_sessions: f64,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub action: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pricing {
    pub status: String,
    pub source: String,
    #[serde(default)]
    pub fetched_at: Option<String>,
    #[serde(default)]
    pub known_models: f64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub contract_version: f64,
    pub read_at: String,
    pub time_zone: String,
    pub since_day: String,
    pub until_day: String,
    #[serde(deserialize_with = "lenient")]
    pub buckets: Vec<Bucket>,
    #[serde(deserialize_with = "lenient")]
    pub sources: Vec<Source>,
    pub pricing: Pricing,
    #[serde(default)]
    pub scan_duration_ms: f64,
}
impl Summary {
    pub fn from_value(value: &Value) -> Result<Self, String> {
        serde_json::from_value(value.clone()).map_err(|error| format!("The usage summary could not be read: {error}"))
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentUsage {
    pub environment_id: String,
    pub label: String,
    pub summary: Summary,
}

// ---- merged view ------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderTotals {
    pub provider: String,
    pub cost_usd: f64,
    pub total_tokens: f64,
    pub records: f64,
    pub sessions: f64,
    pub cost_share: f64,
    pub token_share: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelTotals {
    pub model: String,
    pub provider: String,
    pub cost_usd: f64,
    pub total_tokens: f64,
    pub tokens: TokenTotals,
    pub records: f64,
    pub unpriced_records: f64,
    pub unpriced_tokens: f64,
    pub cost_share: f64,
    pub token_share: f64,
}
impl ModelTotals {
    /// A model whose every record lacked rates has an unknown cost, not a zero one.
    pub fn cost_unknown(&self) -> bool {
        self.records > 0.0 && self.unpriced_records >= self.records
    }
    pub fn key(&self) -> String {
        format!("{}:{}", self.provider, self.model)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Share {
    pub cost_usd: f64,
    pub total_tokens: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DailyTotals {
    pub day: String,
    pub cost_usd: f64,
    pub total_tokens: f64,
    pub by_provider: IndexMap<String, Share>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HourlyTotals {
    pub day: String,
    pub hour_start: String,
    pub cost_usd: f64,
    pub total_tokens: f64,
    pub by_provider: IndexMap<String, Share>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CostQuality {
    pub provider_reported_share: f64,
    pub model_priced_share: f64,
    pub unpriced_share: f64,
    pub cache_savings_usd: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SplitCost {
    pub input: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub output: f64,
    pub unsplit: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeedCost {
    pub standard: f64,
    pub fast: f64,
    pub ultrafast: f64,
    pub premium: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContractMismatch {
    pub environment_id: String,
    pub direction: &'static str,
    pub contract_version: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MergedUsage {
    pub cost_usd: f64,
    pub uncached_input_tokens: f64,
    pub cached_input_tokens: f64,
    pub cache_creation_tokens: f64,
    pub output_tokens: f64,
    pub reasoning_tokens: f64,
    pub total_tokens: f64,
    pub records: f64,
    pub sessions: f64,
    pub providers: Vec<ProviderTotals>,
    pub models: Vec<ModelTotals>,
    pub daily: Vec<DailyTotals>,
    pub hourly: Vec<HourlyTotals>,
    pub cost_quality: CostQuality,
    pub category_cost: SplitCost,
    pub speed_cost: SpeedCost,
    pub duplicate_sources: Vec<String>,
    pub contributing_environments: Vec<String>,
    pub contract_mismatches: Vec<ContractMismatch>,
}
impl Default for CostQuality {
    fn default() -> Self {
        Self { provider_reported_share: 0.0, model_priced_share: 0.0, unpriced_share: 0.0, cache_savings_usd: 0.0 }
    }
}

pub fn is_compatible_contract_version(version: f64, expected: f64) -> bool {
    version >= USAGE_MERGE_COMPATIBLE_SINCE && version <= expected
}

fn fingerprint_key(fingerprint: &Fingerprint) -> String {
    [fingerprint.host_id.as_str(), &fingerprint.provider, &fingerprint.resolved_home_path, &fingerprint.volume_id].join(" ")
}
fn parse_ms(text: &str) -> f64 {
    DateTime::parse_from_rfc3339(text).map(|at| at.timestamp_millis() as f64).unwrap_or(0.0)
}
fn bucket_tokens(bucket: &Bucket) -> f64 {
    // `reasoning_tokens` is a subset of the output tokens and must not be added again.
    bucket.totals.uncached_input_tokens + bucket.totals.cached_input_tokens + bucket.totals.cache_creation_tokens + bucket.totals.output_tokens
}
fn bucket_key(bucket: &Bucket) -> String {
    json!([bucket.day, bucket.hour_start, bucket.provider, bucket.model]).to_string()
}
fn buckets_for_source(summary: &Summary, source: &Source) -> Vec<usize> {
    let provider_sources = summary.sources.iter().filter(|entry| entry.fingerprint.provider == source.fingerprint.provider).count();
    summary
        .buckets
        .iter()
        .enumerate()
        .filter(|(_, bucket)| {
            bucket.provider == source.fingerprint.provider
                && (bucket.source_path.as_deref() == Some(source.fingerprint.resolved_home_path.as_str()) || (bucket.source_path.is_none() && provider_sources == 1))
        })
        .map(|(index, _)| index)
        .collect()
}

struct Claims {
    owner: HashMap<String, usize>,
    supplemental: HashMap<usize, HashSet<usize>>,
    sessions: HashMap<String, f64>,
    duplicates: Vec<String>,
}

/// Decides which environment owns each physical transcript directory, so several environments
/// on one machine do not count every token once each. A complete scan takes a directory ahead
/// of a partial one; the freshest read wins within a status; ids break ties.
fn claim_sources(environments: &[&EnvironmentUsage]) -> Claims {
    let mut owner: HashMap<String, usize> = HashMap::new();
    let mut owner_source: HashMap<String, (usize, usize)> = HashMap::new();
    let mut seen_by_fingerprint: HashMap<String, HashSet<String>> = HashMap::new();
    let mut supplemental: HashMap<usize, HashSet<usize>> = HashMap::new();
    let mut sessions: HashMap<String, f64> = HashMap::new();
    let mut duplicates = Vec::new();
    let mut ordered: Vec<usize> = (0..environments.len()).collect();
    ordered.sort_by(|&a, &b| {
        let (left, right) = (parse_ms(&environments[a].summary.read_at), parse_ms(&environments[b].summary.read_at));
        right.partial_cmp(&left).unwrap_or(std::cmp::Ordering::Equal).then_with(|| environments[a].environment_id.cmp(&environments[b].environment_id))
    });
    for status in ["ok", "partial", "failed"] {
        for &index in &ordered {
            let environment = environments[index];
            for (source_index, source) in environment.summary.sources.iter().enumerate() {
                if source.status != status {
                    continue;
                }
                let key = fingerprint_key(&source.fingerprint);
                if owner.contains_key(&key) {
                    duplicates.push(format!("{}: {}", environment.label, source.fingerprint.resolved_home_path));
                    continue;
                }
                owner.insert(key.clone(), index);
                owner_source.insert(key.clone(), (index, source_index));
                sessions.insert(key, source.distinct_sessions);
            }
        }
    }
    // A newer partial scan may hold usage recorded after an older complete one; its cells
    // absent from the complete scan are kept.
    for &index in &ordered {
        let environment = environments[index];
        for source in &environment.summary.sources {
            if source.status != "partial" {
                continue;
            }
            let key = fingerprint_key(&source.fingerprint);
            let Some(&(owner_index, owner_source_index)) = owner_source.get(&key) else { continue };
            let owner_environment = environments[owner_index];
            let owner_scan = &owner_environment.summary.sources[owner_source_index];
            if owner_scan.status != "ok" || parse_ms(&environment.summary.read_at) <= parse_ms(&owner_environment.summary.read_at) {
                continue;
            }
            let seen = seen_by_fingerprint.entry(key.clone()).or_insert_with(|| {
                buckets_for_source(&owner_environment.summary, owner_scan).into_iter().map(|bucket| bucket_key(&owner_environment.summary.buckets[bucket])).collect()
            });
            let mut added = false;
            for bucket in buckets_for_source(&environment.summary, source) {
                let cell = bucket_key(&environment.summary.buckets[bucket]);
                if seen.contains(&cell) {
                    continue;
                }
                seen.insert(cell);
                supplemental.entry(index).or_default().insert(bucket);
                added = true;
            }
            if !added {
                continue;
            }
            let current = sessions.get(&key).copied().unwrap_or(0.0);
            sessions.insert(key, current.max(source.distinct_sessions));
        }
    }
    Claims { owner, supplemental, sessions, duplicates }
}

/// Merges every environment's summary. An incompatible server's data is left out and the
/// direction of the mismatch reported, so the page can say which side needs updating.
pub fn merge_usage(environments: &[EnvironmentUsage], expected_version: f64) -> MergedUsage {
    if environments.is_empty() {
        return MergedUsage::default();
    }
    let mut current: Vec<&EnvironmentUsage> = Vec::new();
    let mut contract_mismatches = Vec::new();
    for environment in environments {
        if is_compatible_contract_version(environment.summary.contract_version, expected_version) {
            current.push(environment);
        } else {
            contract_mismatches.push(ContractMismatch {
                environment_id: environment.environment_id.clone(),
                direction: if environment.summary.contract_version < expected_version { "serverBehind" } else { "clientBehind" },
                contract_version: environment.summary.contract_version,
            });
        }
    }
    let claims = claim_sources(&current);
    let (mut cost_usd, mut uncached, mut cached, mut creation, mut output, mut reasoning) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
    let (mut records, mut sessions, mut cache_savings, mut reported_records, mut unpriced_records) = (0.0, 0.0, 0.0, 0.0, 0.0);
    let mut category = CategoryCost::default();
    let (mut fast, mut ultrafast, mut premium) = (0.0, 0.0, 0.0);
    struct ProviderAcc {
        cost_usd: f64,
        total_tokens: f64,
        records: f64,
        sessions: f64,
    }
    struct ModelAcc {
        provider: String,
        cost_usd: f64,
        total_tokens: f64,
        tokens: TokenTotals,
        records: f64,
        unpriced_records: f64,
        unpriced_tokens: f64,
    }
    let mut providers: IndexMap<String, ProviderAcc> = IndexMap::new();
    let mut models: IndexMap<String, ModelAcc> = IndexMap::new();
    let mut daily: IndexMap<String, (f64, f64, IndexMap<String, Share>)> = IndexMap::new();
    let mut hourly: IndexMap<String, (String, f64, f64, IndexMap<String, Share>)> = IndexMap::new();
    let mut contributing = Vec::new();
    for (index, environment) in current.iter().enumerate() {
        // Sources this environment owns after the claims, with their buckets.
        let mut owned_providers: HashSet<&str> = HashSet::new();
        let mut owned_sources: HashSet<(String, String)> = HashSet::new();
        let mut sessions_by_provider: IndexMap<String, f64> = IndexMap::new();
        for source in &environment.summary.sources {
            if source.status == "missing" {
                continue;
            }
            let key = fingerprint_key(&source.fingerprint);
            if claims.owner.get(&key) == Some(&index) {
                let provider = &source.fingerprint.provider;
                owned_providers.insert(provider);
                owned_sources.insert((provider.clone(), source.fingerprint.resolved_home_path.clone()));
                *sessions_by_provider.entry(provider.clone()).or_insert(0.0) += claims.sessions.get(&key).copied().unwrap_or(source.distinct_sessions);
            }
        }
        let supplemental = claims.supplemental.get(&index);
        let buckets: Vec<&Bucket> = environment
            .summary
            .buckets
            .iter()
            .enumerate()
            .filter(|(position, bucket)| {
                supplemental.is_some_and(|set| set.contains(position))
                    || match &bucket.source_path {
                        None => owned_providers.contains(bucket.provider.as_str()),
                        Some(path) => owned_sources.contains(&(bucket.provider.clone(), path.clone())),
                    }
            })
            .map(|(_, bucket)| bucket)
            .collect();
        if !buckets.is_empty() {
            contributing.push(environment.environment_id.clone());
        }
        for (kind, provider_sessions) in &sessions_by_provider {
            sessions += provider_sessions;
            if *provider_sessions == 0.0 {
                continue;
            }
            providers.entry(kind.clone()).or_insert(ProviderAcc { cost_usd: 0.0, total_tokens: 0.0, records: 0.0, sessions: 0.0 }).sessions += provider_sessions;
        }
        for bucket in buckets {
            let tokens = bucket_tokens(bucket);
            cost_usd += bucket.cost_usd;
            cache_savings += bucket.cache_savings_usd;
            uncached += bucket.totals.uncached_input_tokens;
            cached += bucket.totals.cached_input_tokens;
            creation += bucket.totals.cache_creation_tokens;
            output += bucket.totals.output_tokens;
            reasoning += bucket.totals.reasoning_tokens;
            records += bucket.records;
            unpriced_records += bucket.unpriced_records;
            if bucket.cost_source == "providerReported" {
                reported_records += bucket.records;
            }
            if let Some(split) = &bucket.category_cost_usd {
                category.input += split.input;
                category.cache_read += split.cache_read;
                category.cache_write += split.cache_write;
                category.output += split.output;
            }
            fast += bucket.fast_cost_usd.unwrap_or(0.0);
            ultrafast += bucket.ultrafast_cost_usd.unwrap_or(0.0);
            premium += bucket.speed_premium_usd.unwrap_or(0.0);

            let provider = providers.entry(bucket.provider.clone()).or_insert(ProviderAcc { cost_usd: 0.0, total_tokens: 0.0, records: 0.0, sessions: 0.0 });
            provider.cost_usd += bucket.cost_usd;
            provider.total_tokens += tokens;
            provider.records += bucket.records;

            let model = models.entry(format!("{} {}", bucket.provider, bucket.model)).or_insert_with(|| ModelAcc {
                provider: bucket.provider.clone(),
                cost_usd: 0.0,
                total_tokens: 0.0,
                tokens: TokenTotals::default(),
                records: 0.0,
                unpriced_records: 0.0,
                unpriced_tokens: 0.0,
            });
            model.cost_usd += bucket.cost_usd;
            model.total_tokens += tokens;
            model.tokens = TokenTotals {
                uncached_input_tokens: model.tokens.uncached_input_tokens + bucket.totals.uncached_input_tokens,
                cached_input_tokens: model.tokens.cached_input_tokens + bucket.totals.cached_input_tokens,
                cache_creation_tokens: model.tokens.cache_creation_tokens + bucket.totals.cache_creation_tokens,
                output_tokens: model.tokens.output_tokens + bucket.totals.output_tokens,
                reasoning_tokens: model.tokens.reasoning_tokens + bucket.totals.reasoning_tokens,
            };
            model.records += bucket.records;
            model.unpriced_records += bucket.unpriced_records;
            if bucket.records > 0.0 {
                model.unpriced_tokens += (tokens * bucket.unpriced_records) / bucket.records;
            }

            let day = daily.entry(bucket.day.clone()).or_insert_with(|| (0.0, 0.0, IndexMap::new()));
            day.0 += bucket.cost_usd;
            day.1 += tokens;
            let share = day.2.entry(bucket.provider.clone()).or_default();
            share.cost_usd += bucket.cost_usd;
            share.total_tokens += tokens;

            if let Some(start) = &bucket.hour_start {
                let hour = hourly.entry(start.clone()).or_insert_with(|| (bucket.day.clone(), 0.0, 0.0, IndexMap::new()));
                hour.1 += bucket.cost_usd;
                hour.2 += tokens;
                let share = hour.3.entry(bucket.provider.clone()).or_default();
                share.cost_usd += bucket.cost_usd;
                share.total_tokens += tokens;
            }
        }
    }
    let total_tokens = uncached + cached + creation + output;
    let share_of = |part: f64, whole: f64| if whole == 0.0 { 0.0 } else { part / whole };
    let mut provider_rows: Vec<ProviderTotals> = providers
        .into_iter()
        .map(|(provider, totals)| ProviderTotals {
            provider,
            cost_usd: totals.cost_usd,
            total_tokens: totals.total_tokens,
            records: totals.records,
            sessions: totals.sessions,
            cost_share: share_of(totals.cost_usd, cost_usd),
            token_share: share_of(totals.total_tokens, total_tokens),
        })
        .collect();
    provider_rows.sort_by(|a, b| cmp_desc(a.cost_usd, b.cost_usd));
    let mut model_rows: Vec<ModelTotals> = models
        .into_iter()
        .map(|(key, totals)| ModelTotals {
            model: key[key.find(' ').map_or(0, |at| at + 1)..].to_owned(),
            provider: totals.provider,
            cost_usd: totals.cost_usd,
            total_tokens: totals.total_tokens,
            tokens: totals.tokens,
            records: totals.records,
            unpriced_records: totals.unpriced_records,
            unpriced_tokens: totals.unpriced_tokens,
            cost_share: share_of(totals.cost_usd, cost_usd),
            token_share: share_of(totals.total_tokens, total_tokens),
        })
        .collect();
    model_rows.sort_by(|a, b| cmp_desc(a.cost_usd, b.cost_usd).then_with(|| cmp_desc(a.total_tokens, b.total_tokens)));
    let mut daily_rows: Vec<DailyTotals> = daily.into_iter().map(|(day, (cost, tokens, by_provider))| DailyTotals { day, cost_usd: cost, total_tokens: tokens, by_provider }).collect();
    daily_rows.sort_by(|a, b| a.day.cmp(&b.day));
    let mut hourly_rows: Vec<HourlyTotals> =
        hourly.into_iter().map(|(hour_start, (day, cost, tokens, by_provider))| HourlyTotals { day, hour_start, cost_usd: cost, total_tokens: tokens, by_provider }).collect();
    hourly_rows.sort_by(|a, b| a.hour_start.cmp(&b.hour_start));
    MergedUsage {
        cost_usd,
        uncached_input_tokens: uncached,
        cached_input_tokens: cached,
        cache_creation_tokens: creation,
        output_tokens: output,
        reasoning_tokens: reasoning,
        total_tokens,
        records,
        sessions,
        providers: provider_rows,
        models: model_rows,
        daily: daily_rows,
        hourly: hourly_rows,
        cost_quality: CostQuality {
            provider_reported_share: if records == 0.0 { 0.0 } else { reported_records / records },
            unpriced_share: if records == 0.0 { 0.0 } else { unpriced_records / records },
            model_priced_share: if records == 0.0 { 0.0 } else { (records - reported_records - unpriced_records) / records },
            cache_savings_usd: cache_savings,
        },
        // Clamped so float error never shows as a negative remainder.
        category_cost: SplitCost {
            input: category.input,
            cache_read: category.cache_read,
            cache_write: category.cache_write,
            output: category.output,
            unsplit: (cost_usd - category.input - category.cache_read - category.cache_write - category.output).max(0.0),
        },
        speed_cost: SpeedCost { standard: (cost_usd - fast - ultrafast).max(0.0), fast, ultrafast, premium },
        duplicate_sources: claims.duplicates,
        contributing_environments: contributing,
        contract_mismatches,
    }
}
/// `b - a || ...` of a descending sort: a difference of zero (or NaN) falls through to the next key.
fn cmp_desc(a: f64, b: f64) -> std::cmp::Ordering {
    let difference = b - a;
    if difference > 0.0 {
        std::cmp::Ordering::Greater
    } else if difference < 0.0 {
        std::cmp::Ordering::Less
    } else {
        std::cmp::Ordering::Equal
    }
}

// ---- number formats ---------------------------------------------------------------------

/// `Number.prototype.toFixed`: the exact value rounded to `digits` places, ties away from zero.
pub fn fixed(value: f64, digits: usize) -> String {
    if value.is_nan() {
        return "NaN".into();
    }
    if value.is_infinite() {
        return if value < 0.0 { "-Infinity" } else { "Infinity" }.into();
    }
    if value < 0.0 {
        let positive = fixed(-value, digits);
        return format!("-{positive}");
    }
    round_decimal(&format!("{:.*}", digits + 60, value), digits)
}
/// What `Intl.NumberFormat` does instead: it rounds the shortest decimal that reads back as the
/// number (2.675 is "2.675", so it rounds to 2.68), ties away from zero.
fn fixed_shortest(value: f64, digits: usize) -> String {
    round_decimal(&format!("{value}"), digits)
}
/// Rounds a plain non-negative decimal string to `digits` places, halves up.
fn round_decimal(text: &str, digits: usize) -> String {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    let mut digits_text: Vec<u8> = whole.bytes().chain(fraction.bytes().chain(std::iter::repeat(b'0')).take(digits)).collect();
    let round_up = fraction.as_bytes().get(digits).is_some_and(|digit| *digit >= b'5');
    if round_up {
        let mut at = digits_text.len();
        loop {
            if at == 0 {
                digits_text.insert(0, b'1');
                break;
            }
            at -= 1;
            if digits_text[at] == b'9' {
                digits_text[at] = b'0';
            } else {
                digits_text[at] += 1;
                break;
            }
        }
    }
    let text = String::from_utf8(digits_text).expect("ascii digits");
    let split = text.len() - digits;
    if digits == 0 { text } else { format!("{}.{}", &text[..split], &text[split..]) }
}
/// `Math.round`: halves go toward positive infinity.
fn round_half_up(value: f64) -> f64 {
    let rounded = (value + 0.5).floor();
    // Like `Math.round`, a negative that rounds to zero is negative zero.
    if rounded == 0.0 && value < 0.0 { -0.0 } else { rounded }
}
fn grouped(whole: &str) -> String {
    let mut out = String::new();
    for (index, digit) in whole.chars().enumerate() {
        if index > 0 && (whole.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }
    out
}
pub fn format_usd(value: f64) -> String {
    let text = fixed_shortest(value.abs(), 2);
    let (whole, fraction) = text.split_once('.').unwrap_or((&text, "00"));
    format!("{}${}.{}", if value.is_sign_negative() { "-" } else { "" }, grouped(whole), fraction)
}
pub fn format_count(value: f64) -> String {
    let rounded = round_half_up(value);
    let whole = fixed_shortest(rounded.abs(), 0);
    format!("{}{}", if rounded.is_sign_negative() { "-" } else { "" }, grouped(&whole))
}
fn trim(value: f64) -> String {
    let magnitude = value.abs();
    let digits = if magnitude >= 100.0 { 0 } else if magnitude >= 10.0 { 1 } else { 2 };
    let text = fixed(value, digits);
    // `replace(/\.0+$/, "")`
    match text.split_once('.') {
        Some((whole, fraction)) if fraction.bytes().all(|byte| byte == b'0') => whole.to_owned(),
        _ => text,
    }
}
/// Three significant figures with a unit, so columns of numbers line up (`19.9B`, `804K`).
pub fn format_tokens(value: f64) -> String {
    let magnitude = value.abs();
    if magnitude >= 1e12 {
        format!("{}T", trim(value / 1e12))
    } else if magnitude >= 1e9 {
        format!("{}B", trim(value / 1e9))
    } else if magnitude >= 1e6 {
        format!("{}M", trim(value / 1e6))
    } else if magnitude >= 1e3 {
        format!("{}K", trim(value / 1e3))
    } else {
        format_count(value)
    }
}
pub fn format_percent(share: f64, digits: usize) -> String {
    let percent = share * 100.0;
    let smallest = 10f64.powi(-(digits as i32));
    if percent > 0.0 && percent < smallest {
        return format!("<{}%", fixed(smallest, digits));
    }
    format!("{}%", fixed(percent, digits))
}
pub fn format_contract_mismatch(label: &str, direction: &str) -> String {
    if direction == "serverBehind" {
        format!("{label} runs an older server version and is excluded from totals.")
    } else {
        format!("This client is older than the server on {label}; its usage is excluded from totals.")
    }
}

// ---- dates -------------------------------------------------------------------------------

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
const HOUR_MS: i64 = 3_600_000;

/// `2026-08-07` to `Aug 7`.
pub fn format_day_short(day: &str) -> String {
    let parts: Vec<Option<f64>> = day.split('-').map(|part| part.trim().parse::<f64>().ok()).collect();
    let (Some(Some(_)), Some(Some(month)), Some(Some(day_of_month))) = (parts.first(), parts.get(1), parts.get(2)) else { return day.to_owned() };
    let name = if (1.0..=12.0).contains(month) && month.fract() == 0.0 { MONTHS[*month as usize - 1] } else { "" };
    format!("{name} {day_of_month}")
}
fn instant_ms(text: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(text).ok().map(|at| at.timestamp_millis())
}
/// Inclusive day list between two `YYYY-MM-DD` bounds.
pub fn enumerate_days(since: &str, until: &str) -> Vec<String> {
    let (Ok(start), Ok(end)) = (NaiveDate::parse_from_str(since, "%Y-%m-%d"), NaiveDate::parse_from_str(until, "%Y-%m-%d")) else { return Vec::new() };
    let mut days = Vec::new();
    let mut cursor = start;
    while cursor <= end {
        days.push(cursor.format("%Y-%m-%d").to_string());
        cursor += Duration::days(1);
    }
    days
}
fn iso(ms: i64) -> String {
    DateTime::from_timestamp_millis(ms).map(|at| at.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()).unwrap_or_default()
}
/// Every fixed-duration bucket start in an hourly rolling window.
pub fn enumerate_hour_starts(since: &str, until: &str) -> Vec<String> {
    let (Some(start), Some(end)) = (instant_ms(since), instant_ms(until)) else { return Vec::new() };
    if end <= start {
        return Vec::new();
    }
    (0..).map(|step| start + step * HOUR_MS).take_while(|cursor| *cursor < end).map(iso).collect()
}

/// A time zone: its name, and its offset from UTC in minutes east at an instant.
#[derive(Clone, Copy)]
pub struct Zone<'a> {
    pub name: &'a str,
    pub offset: &'a dyn Fn(i64) -> i32,
}
impl Zone<'_> {
    fn local(&self, ms: i64) -> chrono::NaiveDateTime {
        DateTime::from_timestamp_millis(ms + (self.offset)(ms) as i64 * 60_000).unwrap_or_default().naive_utc()
    }
    fn wall_hour(&self, ms: i64) -> (i32, u32, u32, u32) {
        let local = self.local(ms);
        (local.year(), local.month(), local.day(), local.hour())
    }
    /// What `Intl` calls the short name in `en-US`: the United States zones' letters, `GMT+h` elsewhere.
    fn short_name(&self, ms: i64) -> String {
        let offset = (self.offset)(ms);
        let named = match (self.name, offset) {
            ("America/New_York", -240) => Some("EDT"),
            ("America/New_York", -300) => Some("EST"),
            ("America/Chicago", -300) => Some("CDT"),
            ("America/Chicago", -360) => Some("CST"),
            ("America/Denver", -360) => Some("MDT"),
            ("America/Denver", -420) => Some("MST"),
            ("America/Los_Angeles", -420) => Some("PDT"),
            ("America/Los_Angeles", -480) => Some("PST"),
            ("UTC" | "Etc/UTC", 0) => Some("UTC"),
            _ => None,
        };
        if let Some(name) = named {
            return name.to_owned();
        }
        let (sign, magnitude) = if offset < 0 { ('-', -offset) } else { ('+', offset) };
        match (magnitude / 60, magnitude % 60) {
            (0, 0) => "GMT".to_owned(),
            (hours, 0) => format!("GMT{sign}{hours}"),
            (hours, minutes) => format!("GMT{sign}{hours}:{minutes:02}"),
        }
    }
}
fn twelve_hour(hour: u32) -> String {
    format!("{} {}", if hour % 12 == 0 { 12 } else { hour % 12 }, if hour < 12 { "AM" } else { "PM" })
}
/// A rolling bucket start in the viewer's zone. A wall-clock hour that happens twice (the
/// clocks going back) carries its zone's short name so the two stay distinguishable.
pub fn format_hour_short(hour_start: &str, zone: Zone) -> String {
    let Some(ms) = instant_ms(hour_start) else { return hour_start.to_owned() };
    let wall = zone.wall_hour(ms);
    let repeated = [-HOUR_MS, HOUR_MS].iter().any(|offset| zone.wall_hour(ms + offset) == wall);
    let base = twelve_hour(wall.3);
    if repeated { format!("{base} {}", zone.short_name(ms)) } else { base }
}
/// `2026-08-11T14:37:00Z` to `Aug 11, 2 PM` in the zone.
pub fn format_date_time_short(instant: &str, zone: Zone) -> String {
    let Some(ms) = instant_ms(instant) else { return instant.to_owned() };
    let local = zone.local(ms);
    format!("{} {}, {}", MONTHS[local.month0() as usize], local.day(), twelve_hour(local.hour()))
}
/// An hourly tooltip label relative to the rolling window's end.
pub fn format_relative_hour_short(hour_start: &str, relative_to: &str, zone: Zone) -> String {
    let (Some(instant), Some(reference)) = (instant_ms(hour_start), instant_ms(relative_to)) else { return format_date_time_short(hour_start, zone) };
    let day_of = |ms: i64| zone.local(ms).date();
    let days_ago = (day_of(reference) - day_of(instant)).num_days();
    let hour = format_hour_short(hour_start, zone);
    match days_ago {
        0 => format!("{hour} today"),
        1 => format!("{hour} yesterday"),
        _ => format_date_time_short(hour_start, zone),
    }
}

/// What the page asks the server for, in the viewer's own zone so days line up with what they
/// experienced.
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub since_day: String,
    pub until_day: String,
    pub time_zone: String,
    pub hourly: bool,
    pub since_time: Option<String>,
    pub until_time: Option<String>,
}
impl Window {
    pub fn to_value(&self) -> Value {
        let mut value = json!({"sinceDay":self.since_day,"untilDay":self.until_day,"timeZone":self.time_zone,"resolution":if self.hourly {"hour"} else {"day"}});
        if let (Some(since), Some(until)) = (&self.since_time, &self.until_time) {
            value["sinceTime"] = json!(since);
            value["untilTime"] = json!(until);
        }
        value
    }
}
pub fn make_window(days: i64, now_ms: i64, zone: Zone, hourly: bool) -> Window {
    let day_text = |ms: i64| zone.local(ms).format("%Y-%m-%d").to_string();
    let until_day = day_text(now_ms);
    if hourly {
        // Minute-aligned bounds: an exact rolling 24 hours, correct across offset changes.
        let until = now_ms.div_euclid(60_000) * 60_000;
        let since = until - 24 * HOUR_MS;
        return Window { since_day: day_text(since), until_day: day_text(until), time_zone: zone.name.to_owned(), hourly, since_time: Some(iso(since)), until_time: Some(iso(until)) };
    }
    // Calendar arithmetic on the local end day, where days are uniform.
    let end = NaiveDate::parse_from_str(&until_day, "%Y-%m-%d").unwrap_or_default();
    Window { since_day: (end - Duration::days(days - 1)).format("%Y-%m-%d").to_string(), until_day, time_zone: zone.name.to_owned(), hourly, since_time: None, until_time: None }
}

// ---- providers ---------------------------------------------------------------------------

pub struct ProviderPresentation {
    pub kind: &'static str,
    pub label: &'static str,
    pub color: &'static str,
}
/// Declaration order is the reading order of every chart and table.
pub const PROVIDERS: [ProviderPresentation; 6] = [
    ProviderPresentation { kind: "codex", label: "Codex", color: "var(--contrast-foreground, var(--foreground))" },
    ProviderPresentation { kind: "claude", label: "Claude Code", color: "#d97757" },
    ProviderPresentation { kind: "grok", label: "Grok Build", color: "color-mix(in oklab, var(--contrast-foreground, var(--foreground)) 72%, var(--background))" },
    ProviderPresentation { kind: "cursor", label: "Cursor", color: "#8b8b8b" },
    ProviderPresentation { kind: "opencode", label: "OpenCode", color: "#5b9bbd" },
    ProviderPresentation { kind: "antigravity", label: "Antigravity", color: "#8c7bd1" },
];
pub fn presentation(kind: &str) -> Option<&'static ProviderPresentation> {
    PROVIDERS.iter().find(|provider| provider.kind == kind)
}
/// Providers with real activity, in reading order, whatever metric is shown.
pub fn providers_with_usage(totals: &[ProviderTotals]) -> Vec<&'static str> {
    PROVIDERS.iter().filter(|provider| totals.iter().any(|entry| entry.provider == provider.kind && (entry.total_tokens > 0.0 || entry.cost_usd > 0.0))).map(|provider| provider.kind).collect()
}

// ---- breakdown ---------------------------------------------------------------------------

pub fn sort_models_by_tokens(models: &[ModelTotals]) -> Vec<ModelTotals> {
    let mut sorted = models.to_vec();
    sorted.sort_by(|a, b| cmp_desc(a.total_tokens, b.total_tokens).then_with(|| cmp_desc(a.cost_usd, b.cost_usd)));
    sorted
}
/// A model's share of the metric, or `None` for the cost share of an unknown cost.
pub fn model_share(model: &ModelTotals, tokens: bool) -> Option<f64> {
    if tokens { Some(model.token_share) } else if model.cost_unknown() { None } else { Some(model.cost_share) }
}
/// Share of a model's input read from cache; cache writes count as misses.
pub fn cache_hit_rate(model: &ModelTotals) -> Option<f64> {
    let tokens = &model.tokens;
    let input = tokens.uncached_input_tokens + tokens.cached_input_tokens + tokens.cache_creation_tokens;
    if input == 0.0 { None } else { Some(tokens.cached_input_tokens / input) }
}
/// Effective USD per million priced tokens, or `None` when none were priced.
pub fn cost_per_million_tokens(model: &ModelTotals) -> Option<f64> {
    let priced = model.total_tokens - model.unpriced_tokens;
    if priced <= 0.0 || model.cost_unknown() { None } else { Some((model.cost_usd / priced) * 1_000_000.0) }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Segment {
    pub label: &'static str,
    pub value: f64,
    pub color: String,
}
/// A neutral step between background and foreground, so mixes never borrow a provider's color.
fn ink(percent: u32) -> String {
    format!("color-mix(in oklab, var(--foreground) {percent}%, var(--background))")
}
fn segment(label: &'static str, value: f64, percent: u32) -> Segment {
    Segment { label, value, color: ink(percent) }
}
pub fn cost_type_segments(cost: &SplitCost) -> Vec<Segment> {
    vec![
        segment("Input", cost.input, 60),
        segment("Cache read", cost.cache_read, 30),
        segment("Cache write", cost.cache_write, 72),
        segment("Output", cost.output, 100),
        // Cost with no rates to split it. Below a cent it is rounding, not usage.
        segment("Other", if cost.unsplit >= 0.005 { cost.unsplit } else { 0.0 }, 44),
    ]
}
pub fn token_type_segments(tokens: &TokenTotals) -> Vec<Segment> {
    vec![
        segment("Input", tokens.uncached_input_tokens, 60),
        segment("Cache read", tokens.cached_input_tokens, 30),
        segment("Cache write", tokens.cache_creation_tokens, 72),
        segment("Output", tokens.output_tokens, 100),
    ]
}
pub fn speed_cost_segments(cost: &SpeedCost) -> Vec<Segment> {
    vec![segment("Standard", cost.standard, 34), segment("Fast", cost.fast, 66), segment("Ultrafast", cost.ultrafast, 100)]
}

// ---- chart -------------------------------------------------------------------------------

pub const VIEW_WIDTH: f64 = 960.0;
pub const VIEW_HEIGHT: f64 = 260.0;
pub const TICK_COUNT: f64 = 4.0;
pub const PLOT_TOP: f64 = 8.0;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Band {
    pub provider: &'static str,
    pub value: f64,
}
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Column {
    pub bands: Vec<Band>,
    pub total: f64,
}
/// One value per provider for each period, in the reading order. `by_period` maps a day or an
/// hour start to its providers.
pub fn build_period_columns(periods: &[String], by_period: &HashMap<&str, &IndexMap<String, Share>>, tokens: bool) -> Vec<Column> {
    periods
        .iter()
        .map(|period| {
            let entry = by_period.get(period.as_str());
            let bands: Vec<Band> = PROVIDERS
                .iter()
                .map(|provider| {
                    let value = entry.and_then(|entry| entry.get(provider.kind)).map_or(0.0, |share| if tokens { share.total_tokens } else { share.cost_usd });
                    Band { provider: provider.kind, value }
                })
                .collect();
            let total = bands.iter().fold(0.0, |sum, band| sum + band.value);
            Column { bands, total }
        })
        .collect()
}

/// A scale whose maximum is a readable 1/2/5 x 10^n step at or above the peak. Rounding the
/// maximum up keeps the tallest period from being drawn past the top of the plot.
pub fn nice_scale(peak: f64, count: f64) -> (f64, Vec<f64>) {
    if peak <= 0.0 {
        return (0.0, vec![0.0]);
    }
    let raw_step = peak / count;
    let magnitude = 10f64.powf(raw_step.log10().floor());
    let normalized = raw_step / magnitude;
    let step = (if normalized > 5.0 { 10.0 } else if normalized > 2.0 { 5.0 } else if normalized > 1.0 { 2.0 } else { 1.0 }) * magnitude;
    let max = (peak / step).ceil() * step;
    let mut ticks = Vec::new();
    let mut value = 0.0;
    while value <= max + step * 1e-6 {
        ticks.push(value);
        value += step;
    }
    (max, ticks)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}
/// Shape-preserving cubic tangents that cannot overshoot spiky usage data.
fn monotone_tangents(points: &[Point]) -> Vec<f64> {
    let count = points.len();
    if count < 2 {
        return vec![0.0];
    }
    let slopes: Vec<f64> = (0..count - 1)
        .map(|index| {
            let (dx, dy) = (points[index + 1].x - points[index].x, points[index + 1].y - points[index].y);
            if dx == 0.0 { 0.0 } else { dy / dx }
        })
        .collect();
    let mut tangents = vec![0.0; count];
    tangents[0] = slopes[0];
    tangents[count - 1] = slopes[count - 2];
    for index in 1..count - 1 {
        let (previous, next) = (slopes[index - 1], slopes[index]);
        tangents[index] = if previous * next <= 0.0 { 0.0 } else { (previous + next) / 2.0 };
    }
    for index in 0..count - 1 {
        let slope = slopes[index];
        if slope == 0.0 {
            tangents[index] = 0.0;
            tangents[index + 1] = 0.0;
            continue;
        }
        let (a, b) = (tangents[index] / slope, tangents[index + 1] / slope);
        let magnitude = a * a + b * b;
        if magnitude > 9.0 {
            let scale = 3.0 / magnitude.sqrt();
            tangents[index] = scale * a * slope;
            tangents[index + 1] = scale * b * slope;
        }
    }
    tangents
}
/// An SVG path through the points as a monotone cubic curve; empty for fewer than two points.
pub fn curve_path(points: &[Point]) -> String {
    if points.len() < 2 {
        return String::new();
    }
    let tangents = monotone_tangents(points);
    let mut path = format!("M{},{}", fixed(points[0].x, 2), fixed(points[0].y, 2));
    for index in 0..points.len() - 1 {
        let (from, to) = (points[index], points[index + 1]);
        let dx = to.x - from.x;
        let c1 = Point { x: from.x + dx / 3.0, y: from.y + (tangents[index] * dx) / 3.0 };
        let c2 = Point { x: to.x - dx / 3.0, y: to.y - (tangents[index + 1] * dx) / 3.0 };
        path += &format!(" C{},{} {},{} {},{}", fixed(c1.x, 2), fixed(c1.y, 2), fixed(c2.x, 2), fixed(c2.y, 2), fixed(to.x, 2), fixed(to.y, 2));
    }
    path
}

#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    pub provider: &'static str,
    pub total: f64,
    pub area: String,
    pub line: String,
}
#[derive(Debug, Clone, PartialEq)]
pub struct Chart {
    pub series: Vec<Series>,
    pub ticks: Vec<(f64, f64)>,
    pub step_x: f64,
    pub columns: Vec<Column>,
}
/// The paths, gridlines and per-period readout of the provider chart. The scale tops out at the
/// largest single provider-period, not the sum: layered series each measure from zero.
pub fn build_chart(providers: &[&'static str], columns: Vec<Column>) -> Chart {
    if columns.is_empty() {
        return Chart { series: Vec::new(), ticks: vec![(0.0, VIEW_HEIGHT)], step_x: 0.0, columns };
    }
    let peak = columns.iter().fold(0.0, |max: f64, column| column.bands.iter().fold(max, |inner, band| inner.max(band.value)));
    let (max, tick_values) = nice_scale(peak, TICK_COUNT);
    let step = if columns.len() == 1 { 0.0 } else { VIEW_WIDTH / (columns.len() as f64 - 1.0) };
    let to_y = |value: f64| if max == 0.0 { VIEW_HEIGHT } else { VIEW_HEIGHT - (value / max) * (VIEW_HEIGHT - PLOT_TOP) };
    let mut series: Vec<Series> = providers
        .iter()
        .map(|provider| {
            let index = PROVIDERS.iter().position(|entry| entry.kind == *provider).unwrap_or(0);
            let points: Vec<Point> = columns.iter().enumerate().map(|(position, column)| Point { x: position as f64 * step, y: to_y(column.bands[index].value) }).collect();
            let line = curve_path(&points);
            Series {
                provider,
                total: columns.iter().fold(0.0, |sum, column| sum + column.bands[index].value),
                area: if line.is_empty() { String::new() } else { format!("{line} L{VIEW_WIDTH},{VIEW_HEIGHT} L0,{VIEW_HEIGHT} Z") },
                line,
            }
        })
        .collect();
    // The heavier series is painted first so the lighter one is not buried.
    series.sort_by(|a, b| cmp_desc(a.total, b.total));
    Chart { series, ticks: tick_values.into_iter().map(|tick| (tick, to_y(tick))).collect(), step_x: step, columns }
}

// ---- page controls -----------------------------------------------------------------------

/// The metric buttons: value, label, and the command that selects it.
pub const METRIC_OPTIONS: [(&str, &str, &str); 3] = [("cost", "Cost", "usage.cost"), ("tokens", "Tokens", "usage.tokens"), ("limits", "Limits", "usage.limits")];
/// The period buttons: days, label, and the command that selects it.
pub const WINDOW_OPTIONS: [(i64, &str, &str); 4] = [(1, "Past 24h", "usage.period.day"), (7, "7 days", "usage.period.week"), (30, "30 days", "usage.period.month"), (90, "90 days", "usage.period.quarter")];
pub const PREFERENCES_KEY: &str = "t3code:usage-page-preferences:v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preferences {
    pub metric: &'static str,
    pub window_days: i64,
}
impl Default for Preferences {
    /// Limits is what most people open the page for, so it is the first-visit default.
    fn default() -> Self {
        Self { metric: "limits", window_days: 30 }
    }
}
impl Preferences {
    pub fn read(text: Option<&str>) -> Self {
        let Some(value) = text.and_then(|text| serde_json::from_str::<Value>(text).ok()) else { return Self::default() };
        let metric = METRIC_OPTIONS.iter().find(|option| value["metric"] == option.0).map(|option| option.0);
        let days = WINDOW_OPTIONS.iter().find(|option| value["windowDays"].as_i64() == Some(option.0)).map(|option| option.0);
        match (metric, days) {
            (Some(metric), Some(window_days)) => Self { metric, window_days },
            _ => Self::default(),
        }
    }
    pub fn write(&self) -> String {
        json!({"metric":self.metric,"windowDays":self.window_days}).to_string()
    }
}
