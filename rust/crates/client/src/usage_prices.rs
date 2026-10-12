//! Custom model prices and model mappings of the Usage page: the table's cells over the environments it applies to, which
//! edits become which settings changes, and what a save of them sends. Port of `usagePriceForm.ts`, `usagePriceTable.ts` and
//! `usagePriceTargets.ts` (`apps/web/src/components/usage`). The dialog that draws it is `usage_prices.rs` in `t3-ui`.
//!
//! A price is per million tokens in US dollars. A mapped model's usage counts as the model it maps to, so mapping a model drops
//! its own price. Blank cache rates use the input price; an explicit 0 means free.
use regex::Regex;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::sync::OnceLock;

/// A price field of the table: the settings key, the column's label and whether it may stay blank.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field {
    pub key: &'static str,
    pub label: &'static str,
    pub optional: bool,
}
pub const FIELDS: [Field; 4] = [
    Field { key: "inputCostPerMillionTokens", label: "Input", optional: false },
    Field { key: "outputCostPerMillionTokens", label: "Output", optional: false },
    Field { key: "cacheReadCostPerMillionTokens", label: "Cache read", optional: true },
    Field { key: "cacheWriteCostPerMillionTokens", label: "Cache write", optional: true },
];

/// A model's custom price as the settings hold it: a rate for each field, the cache ones only when set.
#[derive(Debug, Clone, PartialEq)]
pub struct Price(pub BTreeMap<&'static str, f64>);
impl Price {
    /// The price in a settings value (`UsageModelPriceOverride`), or None when it is not one.
    pub fn read(value: &Value) -> Option<Self> {
        let record = value.as_object()?;
        let mut rates = BTreeMap::new();
        for field in FIELDS {
            match record.get(field.key).and_then(Value::as_f64) {
                Some(rate) => {
                    rates.insert(field.key, rate);
                }
                None if field.optional => {}
                None => return None,
            }
        }
        Some(Self(rates))
    }
    pub fn wire(&self) -> Value {
        Value::Object(self.0.iter().map(|(key, rate)| ((*key).to_owned(), json!(rate))).collect())
    }
}

/// `usagePriceForm` for one field: the rate as the table writes it, empty when the price does not set it.
pub fn form_value(price: Option<&Price>, field: Field) -> String {
    price.and_then(|price| price.0.get(field.key)).map(|rate| format_rate(*rate)).unwrap_or_default()
}
/// A rate as a person types it: no trailing `.0`.
fn format_rate(rate: f64) -> String {
    rate.to_string()
}

/// A form's prices, each field as typed.
pub type Values = BTreeMap<&'static str, String>;

/// `parseUsagePriceForm`: the price a form means, or None when a required rate is blank or a rate is not a non-negative number.
pub fn parse_form(model: &str, form: &Values) -> Option<(String, Price)> {
    static NUMBER: OnceLock<Regex> = OnceLock::new();
    let number = NUMBER.get_or_init(|| Regex::new(r"(?i)^(?:\d+(?:\.\d*)?|\.\d+)(?:e[+-]?\d+)?$").expect("rate pattern"));
    let model = model.trim();
    if model.is_empty() {
        return None;
    }
    let mut rates = BTreeMap::new();
    for field in FIELDS {
        let raw = form.get(field.key).map(|raw| raw.trim()).unwrap_or_default();
        if raw.is_empty() {
            if field.optional {
                continue;
            }
            return None;
        }
        if !number.is_match(raw) {
            return None;
        }
        let rate: f64 = raw.parse().ok()?;
        if !rate.is_finite() || rate < 0.0 {
            return None;
        }
        rates.insert(field.key, rate);
    }
    Some((model.to_owned(), Price(rates)))
}

/// One environment the prices apply to (`UsagePriceTarget`).
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub environment_id: String,
    pub label: String,
    /// `None` when the settings are not loaded.
    pub prices: Option<BTreeMap<String, Price>>,
    /// Model mappings. `None` when they are not loaded or the server cannot store them.
    pub aliases: Option<BTreeMap<String, String>>,
    /// Why this environment cannot be written to now.
    pub unavailable: Option<String>,
}

/// What `unavailable` says of an environment (`priceTargetsAtom`): the connection first, then the settings, then what the server
/// can store, then the grant.
pub fn unavailable_reason(connected: bool, settings_loaded: bool, price_overrides_supported: bool, may_write: bool) -> Option<&'static str> {
    if !connected {
        Some("Offline")
    } else if !settings_loaded {
        Some("Prices not loaded")
    } else if !price_overrides_supported {
        Some("Update server to edit prices")
    } else if !may_write {
        Some("Read-only access")
    } else {
        None
    }
}

/// The prices and mappings in a server's settings value (`settings.usagePriceOverrides`, `settings.usageModelAliases`).
pub fn read_prices(settings: &Value) -> Option<BTreeMap<String, Price>> {
    let record = settings.get("usagePriceOverrides")?.as_object()?;
    Some(record.iter().filter_map(|(model, price)| Some((model.clone(), Price::read(price)?))).collect())
}
pub fn read_aliases(settings: &Value) -> Option<BTreeMap<String, String>> {
    let record = settings.get("usageModelAliases")?.as_object()?;
    Some(record.iter().filter_map(|(model, alias)| Some((model.clone(), alias.as_str()?.to_owned()))).collect())
}

/// A row of the table being edited (`UsagePriceDraft`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Draft {
    pub id: String,
    pub model: String,
    pub is_new: bool,
    pub values: Values,
    /// Edited "Map to" model. Absent keeps each environment's mapping; blank removes it.
    pub alias: Option<String>,
    pub removed: bool,
}
impl Draft {
    pub fn new_row(id: impl Into<String>, model: impl Into<String>) -> Self {
        Self { id: id.into(), model: model.into(), is_new: true, ..Self::default() }
    }
}

/// `isEmptyUsagePriceDraft`: a new row nothing has been typed into.
pub fn is_empty_draft(draft: &Draft) -> bool {
    draft.is_new && draft.model.trim().is_empty() && draft.alias.as_deref().unwrap_or_default().trim().is_empty() && draft.values.values().all(|value| value.trim().is_empty())
}

fn model_price<'a>(target: &'a Target, model: &str) -> Option<&'a Price> {
    target.prices.as_ref()?.get(model)
}
fn model_alias<'a>(target: &'a Target, model: &str) -> Option<&'a String> {
    target.aliases.as_ref()?.get(model)
}

/// What a table cell shows over the selected environments: the value they share, and the hint when it is empty or they differ.
#[derive(Debug, Clone, PartialEq)]
pub struct Cell {
    pub value: String,
    pub placeholder: &'static str,
}

/// `usageAliasCell`: the "Map to" cell across the selected environments, read like a price cell.
pub fn alias_cell(targets: &[Target], model: &str) -> Cell {
    if targets.iter().any(|target| target.aliases.is_none()) {
        return Cell { value: String::new(), placeholder: "Unavailable" };
    }
    let values: Vec<String> = targets.iter().map(|target| model_alias(target, model).cloned().unwrap_or_default()).collect();
    if values.iter().any(|value| Some(value) != values.first()) {
        return Cell { value: String::new(), placeholder: "Mixed" };
    }
    Cell { value: values.first().cloned().unwrap_or_default(), placeholder: "None" }
}

/// `usagePriceCell`.
pub fn price_cell(targets: &[Target], model: &str, field: Field) -> Cell {
    let values: Vec<Option<String>> = targets.iter().map(|target| model_price(target, model).map(|price| form_value(Some(price), field))).collect();
    if targets.iter().any(|target| target.prices.is_none()) {
        return Cell { value: String::new(), placeholder: "Unavailable" };
    }
    if values.iter().any(|value| value != &values[0]) {
        return Cell { value: String::new(), placeholder: "Mixed" };
    }
    Cell {
        value: values[0].clone().unwrap_or_default(),
        placeholder: if values[0].is_none() { "Automatic" } else if field.optional { "Input rate" } else { "0.00" },
    }
}

/// One change of a model's custom price or mapping; `None` removes it.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    Price { model: String, price: Option<Price> },
    Alias { model: String, alias: Option<String> },
}

/// `usagePriceTableChanges`: only edited cells replace rates; untouched cells retain each environment's own values. The errors
/// are keyed by draft id.
pub fn table_changes(target: &Target, drafts: &[Draft]) -> (Vec<Change>, BTreeMap<String, String>) {
    let mut changes = vec![];
    let mut errors = BTreeMap::new();
    for draft in drafts {
        if is_empty_draft(draft) {
            continue;
        }
        let model = draft.model.trim();
        let current_alias = model_alias(target, model);
        if draft.removed {
            if model_price(target, model).is_some() {
                changes.push(Change::Price { model: model.to_owned(), price: None });
            }
            if current_alias.is_some() {
                changes.push(Change::Alias { model: model.to_owned(), alias: None });
            }
            continue;
        }
        // A mapped model is priced as its target, so mapping it drops its own price.
        let alias = draft.alias.as_deref().or(current_alias.map(String::as_str)).unwrap_or_default().trim();
        if !alias.is_empty() {
            if model.is_empty() {
                errors.insert(draft.id.clone(), "Enter a model ID.".to_owned());
            } else if alias == model {
                errors.insert(draft.id.clone(), "Map to a different model.".to_owned());
            } else {
                if Some(alias) != current_alias.map(String::as_str) {
                    changes.push(Change::Alias { model: model.to_owned(), alias: Some(alias.to_owned()) });
                }
                if model_price(target, model).is_some() {
                    changes.push(Change::Price { model: model.to_owned(), price: None });
                }
            }
            continue;
        }
        if current_alias.is_some() {
            changes.push(Change::Alias { model: model.to_owned(), alias: None });
            // Clearing a mapping without entering prices returns to automatic pricing.
            if model_price(target, model).is_none() && draft.values.values().all(|value| value.trim().is_empty()) {
                continue;
            }
        }
        let original: Values = FIELDS.iter().map(|field| (field.key, form_value(model_price(target, model), *field))).collect();
        let mut form = original.clone();
        form.extend(draft.values.iter().map(|(key, value)| (*key, value.clone())));
        match parse_form(model, &form) {
            None => {
                let missing = FIELDS.iter().find(|field| !field.optional && form.get(field.key).is_none_or(|value| value.trim().is_empty()));
                errors.insert(
                    draft.id.clone(),
                    if model.is_empty() {
                        "Enter a model ID.".to_owned()
                    } else if let Some(missing) = missing {
                        format!("{} is required on {}.", missing.label, target.label)
                    } else {
                        "Use non-negative numbers for prices.".to_owned()
                    },
                );
            }
            Some((model, price)) => {
                let next: Values = FIELDS.iter().map(|field| (field.key, form_value(Some(&price), *field))).collect();
                if FIELDS.iter().any(|field| original[field.key] != next[field.key]) {
                    changes.push(Change::Price { model, price: Some(price) });
                }
            }
        }
    }
    (changes, errors)
}

/// `usagePriceTableErrors`: unavailable destinations report a save failure without blocking writable environments.
pub fn table_errors(targets: &[Target], drafts: &[Draft]) -> BTreeMap<String, String> {
    targets.iter().filter(|target| target.unavailable.is_none() && target.prices.is_some()).flat_map(|target| table_changes(target, drafts).1).collect()
}

/// What saving one environment's changes does (`writeUsagePrices`).
#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    /// Fails without a request, with this to say.
    Fail(String),
    /// Nothing to send; the environment counts as saved.
    Nothing,
    /// The `server.updateSettings` input: prices and mappings for one destination go out in a single patch.
    Patch(Value),
}
pub const SAVE_FAILED: &str = "Could not save. Try again.";

pub fn plan_write(target: &Target, changes: &[Change]) -> Plan {
    let mut prices = Map::new();
    let mut aliases = Map::new();
    for change in changes {
        match change {
            Change::Price { model, price } => {
                prices.insert(model.clone(), price.as_ref().map_or(Value::Null, Price::wire));
            }
            Change::Alias { model, alias } => {
                aliases.insert(model.clone(), alias.clone().map_or(Value::Null, Value::String));
            }
        }
    }
    if let Some(reason) = &target.unavailable {
        Plan::Fail(reason.clone())
    } else if !aliases.is_empty() && target.aliases.is_none() {
        Plan::Fail("Update server to map models".to_owned())
    } else if changes.is_empty() {
        Plan::Nothing
    } else {
        let mut patch = Map::new();
        if !prices.is_empty() {
            patch.insert("usagePriceOverrides".to_owned(), Value::Object(prices));
        }
        if !aliases.is_empty() {
            patch.insert("usageModelAliases".to_owned(), Value::Object(aliases));
        }
        Plan::Patch(json!({"patch": patch}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(prices: Value, aliases: Value) -> Target {
        Target {
            environment_id: "env".into(),
            label: "Local".into(),
            prices: read_prices(&json!({"usagePriceOverrides": prices})),
            aliases: if aliases.is_null() { None } else { read_aliases(&json!({"usageModelAliases": aliases})) },
            unavailable: None,
        }
    }
    fn typed(pairs: &[(&'static str, &str)]) -> Values {
        pairs.iter().map(|(key, value)| (*key, (*value).to_owned())).collect()
    }
    fn draft(id: &str, model: &str, pairs: &[(&'static str, &str)]) -> Draft {
        Draft { values: typed(pairs), ..Draft::new_row(id, model) }
    }

    #[test]
    fn a_rate_is_a_non_negative_number_and_blank_cache_rates_follow_the_input_price() {
        let form = typed(&[("inputCostPerMillionTokens", "3"), ("outputCostPerMillionTokens", " 15.5 ")]);
        let (model, price) = parse_form(" gpt-x ", &form).unwrap();
        assert_eq!(model, "gpt-x");
        assert_eq!(price.0.len(), 2, "cache rates stay unset");
        let with_free = typed(&[("inputCostPerMillionTokens", "3"), ("outputCostPerMillionTokens", "15"), ("cacheReadCostPerMillionTokens", "0")]);
        assert_eq!(parse_form("m", &with_free).unwrap().1.0["cacheReadCostPerMillionTokens"], 0.0, "an explicit zero means free");
        for bad in ["-1", "abc", "1,5", "", "1e", "Infinity", "0x10"] {
            let form = typed(&[("inputCostPerMillionTokens", bad), ("outputCostPerMillionTokens", "1")]);
            assert!(parse_form("m", &form).is_none(), "{bad:?}");
        }
        for ok in [".5", "5.", "1e3", "2E-2", "0"] {
            assert!(parse_form("m", &typed(&[("inputCostPerMillionTokens", ok), ("outputCostPerMillionTokens", "1")])).is_some(), "{ok:?}");
        }
        assert!(parse_form("   ", &form).is_none(), "a price needs a model");
    }

    #[test]
    fn cells_show_what_the_environments_share_and_say_when_they_differ_or_are_unavailable() {
        let a = target(json!({"m": {"inputCostPerMillionTokens": 3, "outputCostPerMillionTokens": 15}}), json!({"alias-me": "m"}));
        let mut b = target(json!({"m": {"inputCostPerMillionTokens": 4, "outputCostPerMillionTokens": 15}}), json!({}));
        let input = FIELDS[0];
        assert_eq!(price_cell(std::slice::from_ref(&a), "m", input), Cell { value: "3".into(), placeholder: "0.00" });
        assert_eq!(price_cell(std::slice::from_ref(&a), "m", FIELDS[2]), Cell { value: String::new(), placeholder: "Input rate" });
        assert_eq!(price_cell(std::slice::from_ref(&a), "other", input), Cell { value: String::new(), placeholder: "Automatic" });
        assert_eq!(price_cell(&[a.clone(), b.clone()], "m", input).placeholder, "Mixed");
        assert_eq!(price_cell(&[a.clone(), b.clone()], "m", FIELDS[1]).value, "15", "a field they agree on shows its value");
        assert_eq!(alias_cell(&[a.clone(), b.clone()], "alias-me").placeholder, "Mixed");
        assert_eq!(alias_cell(std::slice::from_ref(&a), "alias-me"), Cell { value: "m".into(), placeholder: "None" });
        b.prices = None;
        b.aliases = None;
        assert_eq!(price_cell(&[a.clone(), b.clone()], "m", input).placeholder, "Unavailable");
        assert_eq!(alias_cell(&[a, b], "m").placeholder, "Unavailable");
    }

    #[test]
    fn editing_a_cell_changes_only_that_rate_and_an_unchanged_row_changes_nothing() {
        let t = target(json!({"m": {"inputCostPerMillionTokens": 3, "outputCostPerMillionTokens": 15}}), json!({}));
        let edit = |values: &[(&'static str, &str)]| Draft { is_new: false, values: typed(values), ..Draft::new_row("model:m", "m") };
        // Output edited: the whole price goes, with the input rate it already had.
        let (changes, errors) = table_changes(&t, &[edit(&[("outputCostPerMillionTokens", "20")])]);
        assert!(errors.is_empty());
        let [Change::Price { model, price: Some(price) }] = changes.as_slice() else { panic!("{changes:?}") };
        assert_eq!((model.as_str(), price.0["inputCostPerMillionTokens"], price.0["outputCostPerMillionTokens"]), ("m", 3.0, 20.0));
        // Typed back to the saved value, there is nothing to save.
        assert!(table_changes(&t, &[edit(&[("outputCostPerMillionTokens", "15.0")])]).0.is_empty());
        // A cleared required rate is an error that names the environment.
        let (changes, errors) = table_changes(&t, &[edit(&[("inputCostPerMillionTokens", "")])]);
        assert!(changes.is_empty());
        assert_eq!(errors["model:m"], "Input is required on Local.");
        let (_, errors) = table_changes(&t, &[edit(&[("inputCostPerMillionTokens", "-1")])]);
        assert_eq!(errors["model:m"], "Use non-negative numbers for prices.");
    }

    #[test]
    fn a_new_row_needs_a_model_and_both_required_rates_and_an_empty_one_is_ignored() {
        let t = target(json!({}), json!({}));
        assert!(is_empty_draft(&draft("new:0", "", &[("inputCostPerMillionTokens", "  ")])));
        assert_eq!(table_changes(&t, &[draft("new:0", "", &[])]), (vec![], BTreeMap::new()));
        let (_, errors) = table_changes(&t, &[draft("new:1", "", &[("inputCostPerMillionTokens", "1")])]);
        assert_eq!(errors["new:1"], "Enter a model ID.");
        let (_, errors) = table_changes(&t, &[draft("new:2", "m", &[("inputCostPerMillionTokens", "1")])]);
        assert_eq!(errors["new:2"], "Output is required on Local.");
        let (changes, errors) = table_changes(&t, &[draft("new:3", "m", &[("inputCostPerMillionTokens", "1"), ("outputCostPerMillionTokens", "2")])]);
        assert!(errors.is_empty() && changes.len() == 1);
    }

    #[test]
    fn mapping_a_model_drops_its_own_price_and_clearing_the_mapping_without_prices_returns_to_automatic() {
        let t = target(json!({"m": {"inputCostPerMillionTokens": 3, "outputCostPerMillionTokens": 15}, "n": {"inputCostPerMillionTokens": 1, "outputCostPerMillionTokens": 1}}), json!({"mapped": "m"}));
        let alias = |model: &str, alias: &str| Draft { is_new: false, alias: Some(alias.to_owned()), ..Draft::new_row(format!("model:{model}"), model) };
        assert_eq!(table_changes(&t, &[alias("n", "m")]).0, vec![Change::Alias { model: "n".into(), alias: Some("m".into()) }, Change::Price { model: "n".into(), price: None }]);
        assert_eq!(table_changes(&t, &[alias("n", "n")]).1["model:n"], "Map to a different model.");
        // The same mapping again is not a change.
        assert!(table_changes(&t, &[alias("mapped", "m")]).0.is_empty());
        assert_eq!(table_changes(&t, &[alias("mapped", "")]).0, vec![Change::Alias { model: "mapped".into(), alias: None }]);
        // Removing a row resets whatever the model had.
        let removed = Draft { is_new: false, removed: true, ..Draft::new_row("model:n", "n") };
        assert_eq!(table_changes(&t, &[removed]).0, vec![Change::Price { model: "n".into(), price: None }]);
    }

    #[test]
    fn a_save_is_one_patch_per_environment_and_an_unavailable_one_fails_without_a_request() {
        let t = target(json!({}), json!({}));
        let changes = vec![Change::Price { model: "m".into(), price: Some(Price([("inputCostPerMillionTokens", 1.0), ("outputCostPerMillionTokens", 2.0)].into_iter().collect()) ) }, Change::Alias { model: "a".into(), alias: Some("m".into()) }, Change::Price { model: "gone".into(), price: None }];
        let Plan::Patch(patch) = plan_write(&t, &changes) else { panic!() };
        assert_eq!(patch, json!({"patch":{"usagePriceOverrides":{"m":{"inputCostPerMillionTokens":1.0,"outputCostPerMillionTokens":2.0},"gone":null},"usageModelAliases":{"a":"m"}}}));
        assert_eq!(plan_write(&t, &[]), Plan::Nothing);
        let mut offline = t.clone();
        offline.unavailable = Some("Offline".into());
        assert_eq!(plan_write(&offline, &changes), Plan::Fail("Offline".into()));
        let old_server = target(json!({}), Value::Null);
        assert_eq!(plan_write(&old_server, &changes), Plan::Fail("Update server to map models".into()));
        assert!(matches!(plan_write(&old_server, &changes[..1]), Plan::Patch(_)), "prices alone need no mapping support");
        assert_eq!(unavailable_reason(true, true, true, true), None);
        assert_eq!(unavailable_reason(false, true, true, true), Some("Offline"));
        assert_eq!(unavailable_reason(true, true, false, false), Some("Update server to edit prices"));
        assert_eq!(unavailable_reason(true, true, true, false), Some("Read-only access"));
    }

    #[test]
    fn errors_are_found_only_on_writable_environments() {
        let writable = target(json!({}), json!({}));
        let mut offline = target(json!({}), json!({}));
        offline.unavailable = Some("Offline".into());
        let drafts = [draft("new:1", "m", &[("inputCostPerMillionTokens", "1")])];
        assert_eq!(table_errors(&[writable], &drafts).len(), 1);
        assert!(table_errors(&[offline], &drafts).is_empty());
    }
}
