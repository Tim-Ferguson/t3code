//! Model rate lookup and cost arithmetic (`usagePricing.ts`). Rates come from LiteLLM's
//! `model_prices_and_context_window.json`, the same table `ccusage` prices against. Everything
//! here is pure: fetching and caching the table lives in `usage_service`.
use crate::usage_transcripts::{Speed, Totals};
use serde_json::{Map, Value};
use std::collections::HashMap;

/// Token rates for one billing speed. All values are USD per token.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TokenRates {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_creation: f64,
}

/// A model's standard rates, plus rates for each faster speed it publishes. A request at a speed
/// with no published rates bills at the standard rates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModelRate {
    pub standard: TokenRates,
    pub fast: Option<TokenRates>,
    pub ultrafast: Option<TokenRates>,
}

pub type RateTable = HashMap<String, ModelRate>;

/// `createOverrideRateTable`: custom IDs keep their case, provider prefix and variant suffix, and
/// apply as entered at every speed. `overrides` are USD per million tokens.
pub fn override_table(overrides: impl IntoIterator<Item = (String, Value)>) -> RateTable {
    overrides
        .into_iter()
        .filter_map(|(model, prices)| {
            let per_million = |name: &str| prices[name].as_f64();
            let input = per_million("inputCostPerMillionTokens")?;
            let output = per_million("outputCostPerMillionTokens")?;
            let standard = TokenRates {
                input: input / 1_000_000.0,
                output: output / 1_000_000.0,
                cache_read: per_million("cacheReadCostPerMillionTokens").unwrap_or(input) / 1_000_000.0,
                cache_creation: per_million("cacheWriteCostPerMillionTokens").unwrap_or(input) / 1_000_000.0,
            };
            Some((model.trim().to_owned(), ModelRate { standard, fast: None, ultrafast: None }))
        })
        .collect()
}

fn finite(value: &Value) -> Option<f64> {
    value.as_f64().filter(|value| value.is_finite())
}

/// Reads one rate set, `suffix` selecting a tier such as `_priority`. `None` without both an input
/// and an output rate. When the standard tier omits cache rates, cached input is priced as plain
/// input rather than as free; a faster tier that omits them keeps the standard tier's ratio.
fn read_token_rates(entry: &Map<String, Value>, suffix: &str, standard: Option<&TokenRates>) -> Option<TokenRates> {
    let get = |name: &str| entry.get(&format!("{name}{suffix}")).and_then(finite);
    let (input, output) = (get("input_cost_per_token")?, get("output_cost_per_token")?);
    let cache = |name: &str, field: fn(&TokenRates) -> f64| get(name).unwrap_or_else(|| standard.filter(|standard| standard.input > 0.0).map_or(input, |standard| field(standard) / standard.input * input));
    Some(TokenRates { input, output, cache_read: cache("cache_read_input_token_cost", |rates| rates.cache_read), cache_creation: cache("cache_creation_input_token_cost", |rates| rates.cache_creation) })
}
fn scale(rates: TokenRates, multiple: f64) -> TokenRates {
    TokenRates { input: rates.input * multiple, output: rates.output * multiple, cache_read: rates.cache_read * multiple, cache_creation: rates.cache_creation * multiple }
}
/// `provider_specific_entry.fast`, e.g. `2` for Claude Opus 5.5.
fn fast_multiplier(entry: &Map<String, Value>) -> Option<f64> {
    entry.get("provider_specific_entry")?.get("fast").and_then(finite).filter(|fast| *fast > 0.0)
}

fn normalize_key(model: &str) -> String {
    model.trim().to_lowercase()
}
fn bare_model_name(key: &str) -> &str {
    key.rsplit_once('/').map_or(key, |(_, name)| name)
}

/// `parseRateTable`: entries without both an input and an output rate are dropped, since a
/// half-priced model would silently under-report cost. A bare name is aliased only when no
/// canonical entry exists and every qualified entry has the same rate.
pub fn parse_rate_table(document: &Value) -> RateTable {
    let mut table = RateTable::new();
    // Insertion order decides which alias candidate is seen first; the document order is kept.
    let mut order: Vec<String> = vec![];
    let Some(document) = document.as_object() else { return table };
    for (name, raw) in document {
        let Some(entry) = raw.as_object() else { continue };
        let Some(standard) = read_token_rates(entry, "", None) else { continue };
        let key = normalize_key(name);
        if key.is_empty() {
            continue;
        }
        let fast = match fast_multiplier(entry) {
            Some(multiple) => Some(scale(standard, multiple)),
            None => read_token_rates(entry, "_priority", Some(&standard)),
        };
        let ultrafast = read_token_rates(entry, "_ultrafast", Some(&standard));
        if table.insert(key.clone(), ModelRate { standard, fast, ultrafast }).is_none() {
            order.push(key);
        }
    }
    // `None` marks a bare name claimed at conflicting rates: no alias for it.
    let mut candidates: Vec<(String, Option<ModelRate>)> = vec![];
    for key in &order {
        let alias = bare_model_name(key);
        if alias.is_empty() || alias == key || table.contains_key(alias) {
            continue;
        }
        let rate = table[key];
        match candidates.iter_mut().find(|(candidate, _)| candidate == alias) {
            None => candidates.push((alias.to_owned(), Some(rate))),
            Some((_, held)) => {
                if held.is_some_and(|held| held != rate) {
                    *held = None;
                }
            }
        }
    }
    for (alias, rate) in candidates {
        if let Some(rate) = rate {
            table.insert(alias, rate);
        }
    }
    table
}

fn rates_at(rate: &ModelRate, speed: Speed) -> TokenRates {
    match speed {
        Speed::Standard => rate.standard,
        Speed::Fast => rate.fast.unwrap_or(rate.standard),
        Speed::Ultrafast => rate.ultrafast.unwrap_or(rate.standard),
    }
}

/// Models never priced regardless of the table: `<synthetic>` marks locally generated messages, and
/// bare family names are ambiguous across generations.
const UNPRICEABLE: [&str; 6] = ["<synthetic>", "synthetic", "opus", "sonnet", "haiku", "fable"];

/// `lookupRate`: a bracketed variant suffix such as `claude-fable-5-1[1m]` is dropped.
pub fn lookup_rate<'a>(table: &'a RateTable, model: &str) -> Option<&'a ModelRate> {
    let key = normalize_key(model);
    let key = key.split_once('[').map_or(key.as_str(), |(base, _)| base);
    let bare = bare_model_name(key);
    if bare.is_empty() || UNPRICEABLE.contains(&bare) {
        return None;
    }
    table.get(key)
}

/// The parts of a transcript record that decide its price.
pub struct Priced<'a> {
    pub model: &'a str,
    pub totals: &'a Totals,
    pub speed: Speed,
    pub reported_cost_usd: Option<f64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CategoryCost {
    pub input: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub output: f64,
}
impl CategoryCost {
    fn sum(&self) -> f64 {
        self.input + self.cache_read + self.cache_write + self.output
    }
}
fn cost_by_category(totals: &Totals, rates: &TokenRates) -> CategoryCost {
    CategoryCost {
        input: totals.uncached_input as f64 * rates.input,
        cache_read: totals.cached_input as f64 * rates.cache_read,
        cache_write: totals.cache_creation as f64 * rates.cache_creation,
        output: totals.output as f64 * rates.output,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CostSource {
    ProviderReported,
    ModelPriced,
    Unpriced,
}
impl CostSource {
    pub fn wire(self) -> &'static str {
        match self {
            CostSource::ProviderReported => "providerReported",
            CostSource::ModelPriced => "modelPriced",
            CostSource::Unpriced => "unpriced",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PricedUsage {
    pub cost_usd: f64,
    pub cost_source: CostSource,
    /// `cost_usd` by token category, `None` when no rates are known to split it.
    pub category_cost_usd: Option<CategoryCost>,
    /// What `cost_usd` exceeds the same tokens at standard rates. `0` without rates.
    pub speed_premium_usd: f64,
}

/// `priceUsage`. A provider-reported cost is kept as is, and split by category and speed in
/// proportion to the model's list rates when those are known. `reasoning` is not charged
/// separately: it is already counted inside `output`.
pub fn price_usage(table: &RateTable, record: &Priced<'_>, overrides: Option<&RateTable>) -> PricedUsage {
    let override_rate = overrides.and_then(|overrides| overrides.get(record.model.trim()));
    let reported = if override_rate.is_none() { record.reported_cost_usd.filter(|cost| cost.is_finite()) } else { None };
    let unsplit = |cost_usd, cost_source| PricedUsage { cost_usd, cost_source, category_cost_usd: None, speed_premium_usd: 0.0 };
    let Some(rate) = override_rate.or_else(|| lookup_rate(table, record.model)) else {
        return reported.map_or_else(|| unsplit(0.0, CostSource::Unpriced), |cost| unsplit(cost, CostSource::ProviderReported));
    };
    let list = cost_by_category(record.totals, &rates_at(rate, record.speed));
    let list_usd = list.sum();
    if let Some(reported) = reported {
        if list_usd <= 0.0 {
            return unsplit(reported, CostSource::ProviderReported);
        }
    }
    let premium = if record.speed == Speed::Standard { 0.0 } else { list_usd - cost_by_category(record.totals, &rate.standard).sum() };
    let scale = reported.map_or(1.0, |reported| reported / list_usd);
    PricedUsage {
        cost_usd: reported.unwrap_or(list_usd),
        cost_source: if reported.is_none() { CostSource::ModelPriced } else { CostSource::ProviderReported },
        category_cost_usd: Some(CategoryCost { input: list.input * scale, cache_read: list.cache_read * scale, cache_write: list.cache_write * scale, output: list.output * scale }),
        speed_premium_usd: premium * scale,
    }
}

/// `cacheSavingsUsd`: what the cached input would have cost at full input rates minus what it cost.
pub fn cache_savings_usd(table: &RateTable, record: &Priced<'_>, overrides: Option<&RateTable>) -> f64 {
    let Some(rate) = overrides.and_then(|overrides| overrides.get(record.model.trim())).or_else(|| lookup_rate(table, record.model)) else { return 0.0 };
    let rates = rates_at(rate, record.speed);
    record.totals.cached_input as f64 * (rates.input - rates.cache_read)
}
