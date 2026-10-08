//! Source-backed model and composer rules shared by the Rust surfaces.
//! Wire entities remain in contracts; these helpers own presentation and dispatch normalization.
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
use t3_contracts::{
    ModelCapabilities, ModelSelection, ProviderInstanceId, ProviderOptionDescriptor,
    ProviderOptionSelection, ProviderOptionSelectionValue, RuntimeMode,
    SelectProviderOptionDescriptorFields, ServerProviderModel, TrimmedNonEmptyString,
};

pub const ANTIGRAVITY_DEFAULT_MODEL: &str = "antigravity-default";

pub fn normalize_model_slug(driver: &str, value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let alias = match (driver, value) {
        ("codex", "gpt-5-codex" | "5.4") => "gpt-5.4",
        ("codex", "5.3" | "gpt-5.3") => "gpt-5.3-codex",
        ("codex", "5.3-spark" | "gpt-5.3-spark") => "gpt-5.3-codex-spark",
        ("cursor", "composer") => "composer-2",
        ("cursor", "composer-1") => "composer-1.5",
        ("cursor", "opus-4.6-thinking" | "opus-4.6") => "claude-opus-4-6",
        ("cursor", "sonnet-4.6-thinking" | "sonnet-4.6") => "claude-sonnet-4-6",
        ("cursor", "opus-4.5-thinking" | "opus-4.5") => "claude-opus-4-5",
        _ => value,
    };
    Some(alias.into())
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelOption {
    pub slug: String,
    pub name: String,
    pub aliases: Vec<String>,
    pub short_name: Option<String>,
    pub sub_provider: Option<String>,
    pub badge: Option<String>,
    pub is_custom: bool,
    pub is_default: bool,
    pub is_legacy: bool,
    pub is_unavailable: bool,
}
impl From<&ServerProviderModel> for ModelOption {
    fn from(model: &ServerProviderModel) -> Self {
        Self {
            slug: model.slug.to_string(),
            name: model.name.to_string(),
            aliases: model
                .aliases
                .as_ref()
                .and_then(Option::as_ref)
                .map(|a| a.iter().map(ToString::to_string).collect())
                .unwrap_or_default(),
            short_name: model
                .short_name
                .as_ref()
                .and_then(Option::as_ref)
                .map(ToString::to_string),
            sub_provider: model
                .sub_provider
                .as_ref()
                .and_then(Option::as_ref)
                .map(ToString::to_string),
            badge: model
                .badge
                .as_ref()
                .and_then(Option::as_ref)
                .map(|_| "new".into()),
            is_custom: model.is_custom,
            is_default: model.is_default.flatten().unwrap_or(false),
            is_legacy: model.is_legacy.flatten().unwrap_or(false),
            is_unavailable: false,
        }
    }
}

pub fn resolve_selectable_model<'a>(
    driver: &str,
    value: &str,
    models: &'a [ModelOption],
) -> Option<&'a str> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    // Exact provider-owned slugs outrank names and both kinds of aliases.
    models
        .iter()
        .find(|m| m.slug == value)
        .or_else(|| {
            models
                .iter()
                .find(|m| m.name.to_lowercase() == value.to_lowercase())
        })
        .or_else(|| {
            models.iter().find(|m| {
                m.aliases
                    .iter()
                    .any(|a| a.to_lowercase() == value.to_lowercase())
            })
        })
        .or_else(|| {
            let slug = normalize_model_slug(driver, value)?;
            models.iter().find(|m| m.slug == slug)
        })
        .map(|m| m.slug.as_str())
}

#[derive(Debug, Clone, PartialEq)]
pub struct CustomModel {
    pub slug: String,
    pub name: String,
    pub capabilities: Option<ModelCapabilities>,
}
/// Opaque instance configuration can contain malformed rows. Keep valid identifiers,
/// discard malformed capabilities, and let the first occurrence own the row.
pub fn read_custom_models(value: &Value) -> Vec<CustomModel> {
    let Some(rows) = value.as_array() else {
        return vec![];
    };
    let mut seen = BTreeSet::new();
    rows.iter()
        .filter_map(|row| {
            let slug = row.as_str().or_else(|| row.get("slug")?.as_str())?.trim();
            if slug.is_empty() || !seen.insert(slug.to_owned()) {
                return None;
            }
            let name = row
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .unwrap_or(slug);
            let capabilities = row
                .get("capabilities")
                .filter(|v| !v.is_null())
                .and_then(|v| serde_json::from_value::<ModelCapabilities>(v.clone()).ok())
                .map(|mut c| {
                    c.option_descriptors =
                        Some(Some(c.option_descriptors.flatten().unwrap_or_default()));
                    c
                });
            Some(CustomModel {
                slug: slug.into(),
                name: name.into(),
                capabilities,
            })
        })
        .collect()
}

/// Built-in rows come from discovery; custom rows come from settings, never a
/// potentially stale discovery mirror. Hidden preferences only hide built-ins.
pub fn build_catalog(
    driver: &str,
    raw: &[ServerProviderModel],
    custom: &[CustomModel],
    hidden: &[String],
    order: &[String],
    selected: Option<&str>,
) -> Vec<ModelOption> {
    let builtin: BTreeSet<_> = raw
        .iter()
        .filter(|m| !m.is_custom)
        .map(|m| m.slug.as_str())
        .collect();
    let mut options: Vec<_> = raw
        .iter()
        .filter(|m| !m.is_custom && !hidden.iter().any(|h| h == m.slug.as_str()))
        .map(ModelOption::from)
        .collect();
    let mut seen = BTreeSet::new();
    if driver != "antigravity" {
        for m in custom
            .iter()
            .filter(|m| {
                m.slug.encode_utf16().count() <= 256
                    && !builtin.contains(m.slug.as_str())
                    && seen.insert(m.slug.clone())
            })
            .take(32)
        {
            options.push(ModelOption {
                slug: m.slug.clone(),
                name: m.name.clone(),
                aliases: vec![],
                short_name: None,
                sub_provider: None,
                badge: None,
                is_custom: true,
                is_default: false,
                is_legacy: false,
                is_unavailable: false,
            });
        }
    }
    let ranks: HashMap<_, _> = order
        .iter()
        .enumerate()
        .map(|(i, slug)| (slug.as_str(), i))
        .collect();
    options.sort_by_key(|m| ranks.get(m.slug.as_str()).copied().unwrap_or(usize::MAX));
    if matches!(driver, "opencode" | "antigravity") {
        if let Some(slug) = selected.map(str::trim).filter(|s| !s.is_empty()) {
            let raw_options: Vec<_> = raw.iter().map(ModelOption::from).collect();
            if !(driver == "antigravity" && slug == ANTIGRAVITY_DEFAULT_MODEL)
                && !hidden.iter().any(|h| h == slug)
                && resolve_selectable_model(driver, slug, &raw_options).is_none()
                && !options.iter().any(|m| m.slug == slug)
            {
                options.push(ModelOption {
                    slug: slug.into(),
                    name: slug.into(),
                    aliases: vec![],
                    short_name: None,
                    sub_provider: None,
                    badge: None,
                    is_custom: false,
                    is_default: false,
                    is_legacy: false,
                    is_unavailable: true,
                });
            }
        }
    }
    options
}

pub fn resolve_catalog_selection<'a>(
    driver: &str,
    options: &'a [ModelOption],
    selected: Option<&str>,
) -> Option<&'a str> {
    selected
        .and_then(|s| resolve_selectable_model(driver, s, options))
        .or_else(|| {
            options
                .iter()
                .find(|m| m.is_default)
                .or_else(|| options.first())
                .map(|m| m.slug.as_str())
        })
}

pub fn descriptor_id(descriptor: &ProviderOptionDescriptor) -> &str {
    match descriptor {
        ProviderOptionDescriptor::Select { fields } => fields.id.as_str(),
        ProviderOptionDescriptor::Boolean { fields } => fields.id.as_str(),
    }
}
fn default_choice(fields: &SelectProviderOptionDescriptorFields) -> Option<TrimmedNonEmptyString> {
    fields
        .options
        .iter()
        .find(|o| o.is_default.flatten().unwrap_or(false))
        .map(|o| o.id.clone())
}
fn resolve_choice(
    fields: &SelectProviderOptionDescriptorFields,
    raw: Option<&str>,
) -> Option<TrimmedNonEmptyString> {
    let raw = raw.map(str::trim).filter(|s| !s.is_empty());
    let fallback = || {
        fields
            .current_value
            .clone()
            .flatten()
            .or_else(|| default_choice(fields))
    };
    let Some(raw) = raw else {
        return fallback();
    };
    if fields.options.is_empty() {
        return TrimmedNonEmptyString::new(raw).ok();
    }
    if fields.options.iter().any(|o| o.id.as_str() == raw) {
        if fields
            .prompt_injected_values
            .as_ref()
            .and_then(Option::as_ref)
            .is_some_and(|a| a.iter().any(|v| v.as_str() == raw))
        {
            return default_choice(fields);
        }
        return TrimmedNonEmptyString::new(raw).ok();
    }
    fallback()
}

pub fn option_descriptors(
    caps: &ModelCapabilities,
    selections: Option<&[ProviderOptionSelection]>,
) -> Vec<ProviderOptionDescriptor> {
    caps.option_descriptors
        .as_ref()
        .and_then(Option::as_ref)
        .into_iter()
        .flatten()
        .map(|d| {
            let raw = selections
                .and_then(|s| s.iter().find(|s| s.id.as_str() == descriptor_id(d)))
                .map(|s| &s.value);
            let mut d = d.clone();
            match &mut d {
                ProviderOptionDescriptor::Boolean { fields } => {
                    if let Some(ProviderOptionSelectionValue::Boolean(v)) = raw {
                        fields.current_value = Some(Some(*v));
                    }
                }
                ProviderOptionDescriptor::Select { fields } => {
                    let raw = match raw {
                        Some(ProviderOptionSelectionValue::String(v)) => Some(v.as_str()),
                        _ => fields
                            .current_value
                            .as_ref()
                            .and_then(Option::as_ref)
                            .map(|s| s.as_str()),
                    };
                    fields.current_value = resolve_choice(fields, raw).map(Some);
                }
            }
            d
        })
        .collect()
}

/// Reported values affect display only and only belong to the matching provider/model.
pub fn option_current_value(
    descriptor: &ProviderOptionDescriptor,
    selection: Option<&ModelSelection>,
    reported: Option<&ModelSelection>,
) -> Option<ProviderOptionSelectionValue> {
    let id = descriptor_id(descriptor);
    let explicit = selection
        .and_then(|s| s.options.as_ref())
        .is_some_and(|s| s.iter().any(|o| o.id.as_str() == id));
    if !explicit {
        if let (Some(selected), Some(reported)) = (selection, reported) {
            if selected.instance_id == reported.instance_id && selected.model == reported.model {
                if let Some(value) = reported
                    .options
                    .as_ref()
                    .and_then(|o| o.iter().find(|o| o.id.as_str() == id))
                {
                    return Some(value.value.clone());
                }
            }
        }
        if id == "variant" && selection.is_some() {
            return None;
        }
    }
    match descriptor {
        ProviderOptionDescriptor::Boolean { fields } => fields
            .current_value
            .flatten()
            .map(ProviderOptionSelectionValue::Boolean),
        ProviderOptionDescriptor::Select { fields } => fields
            .current_value
            .clone()
            .flatten()
            .or_else(|| default_choice(fields))
            .map(ProviderOptionSelectionValue::String),
    }
}

/// Provider defaults can be shown without turning them into explicit dispatch state.
/// Explicit values equal to defaults stay explicit so they can overwrite prior sessions.
pub fn explicit_options(
    descriptors: &[ProviderOptionDescriptor],
    selections: Option<&[ProviderOptionSelection]>,
) -> Option<Vec<ProviderOptionSelection>> {
    let selections = selections?;
    let values: Vec<_> = descriptors
        .iter()
        .filter(|d| selections.iter().any(|s| s.id.as_str() == descriptor_id(d)))
        .filter_map(|d| {
            Some(ProviderOptionSelection {
                id: TrimmedNonEmptyString::new(descriptor_id(d)).ok()?,
                value: option_current_value(d, None, None)?,
            })
        })
        .collect();
    (!values.is_empty()).then_some(values)
}

pub fn model_capabilities(
    driver: &str,
    models: &[ServerProviderModel],
    selected: &str,
    plan_enabled: bool,
) -> ModelCapabilities {
    let options: Vec<_> = models.iter().map(ModelOption::from).collect();
    let selected = resolve_selectable_model(driver, selected, &options);
    let mut caps = models
        .iter()
        .find(|m| Some(m.slug.as_str()) == selected)
        .and_then(|m| m.capabilities.clone())
        .unwrap_or(ModelCapabilities {
            option_descriptors: Some(Some(vec![])),
        });
    if !plan_enabled {
        let descriptors = caps
            .option_descriptors
            .take()
            .flatten()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|mut d| {
                if let ProviderOptionDescriptor::Select { fields } = &mut d {
                    if fields.id.as_str() == "agent" {
                        fields.options.retain(|o| o.id.as_str() != "plan");
                        if fields.options.is_empty() {
                            return None;
                        }
                        if !fields
                            .current_value
                            .as_ref()
                            .and_then(Option::as_ref)
                            .is_some_and(|v| fields.options.iter().any(|o| &o.id == v))
                        {
                            fields.current_value = default_choice(fields)
                                .or_else(|| fields.options.first().map(|o| o.id.clone()))
                                .map(Some);
                        }
                    }
                }
                Some(d)
            })
            .collect();
        caps.option_descriptors = Some(Some(descriptors));
    }
    caps
}

pub fn composer_dispatch_options(
    driver: &str,
    models: &[ServerProviderModel],
    selected: &str,
    options: Option<&[ProviderOptionSelection]>,
    plan_enabled: bool,
) -> Option<Vec<ProviderOptionSelection>> {
    if driver == "opencode"
        && !models
            .iter()
            .any(|m| Some(m.slug.as_str()) == normalize_model_slug(driver, selected).as_deref())
    {
        let values: Vec<_> = options?.iter().filter(|o| plan_enabled || o.id.as_str() != "agent" || !matches!(&o.value, ProviderOptionSelectionValue::String(v) if v.as_str() == "plan")).cloned().collect();
        return (!values.is_empty()).then_some(values);
    }
    let caps = model_capabilities(driver, models, selected, plan_enabled);
    let mut selections = options.unwrap_or_default().to_vec();
    if !selections.iter().any(|o| o.id.as_str() == "fastMode") && caps.option_descriptors.as_ref().and_then(Option::as_ref).is_some_and(|d| d.iter().any(|d| matches!(d, ProviderOptionDescriptor::Boolean {fields} if fields.id.as_str() == "fastMode"))) {
        selections.push(ProviderOptionSelection { id: TrimmedNonEmptyString::new("fastMode").expect("literal"), value: ProviderOptionSelectionValue::Boolean(false) });
    }
    explicit_options(
        &option_descriptors(&caps, Some(&selections)),
        Some(&selections),
    )
}

pub fn model_selection_command(
    current: &ProviderInstanceId,
    next: &ModelSelection,
) -> &'static str {
    if current == &next.instance_id {
        "thread.model-selection.set"
    } else {
        "provider.switch"
    }
}
pub fn model_selections_equal(left: &ModelSelection, right: &ModelSelection) -> bool {
    if left.instance_id != right.instance_id || left.model != right.model {
        return false;
    }
    fn canonical(selection: &ModelSelection) -> Vec<(&str, u8, &str)> {
        let mut values: Vec<_> = selection
            .options
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|o| {
                let (kind, value) = match &o.value {
                    ProviderOptionSelectionValue::String(value) => (0, value.as_str()),
                    ProviderOptionSelectionValue::Boolean(false) => (1, "false"),
                    ProviderOptionSelectionValue::Boolean(true) => (1, "true"),
                };
                (o.id.as_str(), kind, value)
            })
            .collect();
        values.sort_unstable();
        values
    }
    canonical(left) == canonical(right)
}

pub const RUNTIME_MODES: [RuntimeMode; 4] = [
    RuntimeMode::ApprovalRequired,
    RuntimeMode::AutoAcceptEdits,
    RuntimeMode::Auto,
    RuntimeMode::FullAccess,
];
pub fn runtime_modes(supported: Option<&[RuntimeMode]>) -> Vec<RuntimeMode> {
    RUNTIME_MODES
        .iter()
        .copied()
        .filter(|mode| supported.is_none_or(|s| s.is_empty() || s.contains(mode)))
        .collect()
}
pub fn compatible_runtime_mode(
    current: RuntimeMode,
    supported: Option<&[RuntimeMode]>,
) -> RuntimeMode {
    let modes = runtime_modes(supported);
    if modes.contains(&current) {
        current
    } else {
        modes.first().copied().unwrap_or(current)
    }
}
pub fn runtime_mode_label(mode: RuntimeMode) -> &'static str {
    match mode {
        RuntimeMode::ApprovalRequired => "Supervised",
        RuntimeMode::AutoAcceptEdits => "Auto-accept edits",
        RuntimeMode::Auto => "Auto",
        RuntimeMode::FullAccess => "Full access",
    }
}
/// Mobile omits prompt-driven selections and ultracode from the actionable menu,
/// while keeping the stored current value available for display.
pub fn mobile_option_choices(
    descriptor: &ProviderOptionDescriptor,
) -> Vec<t3_contracts::ProviderOptionChoice> {
    match descriptor {
        ProviderOptionDescriptor::Select { fields } => fields
            .options
            .iter()
            .filter(|o| {
                o.id.as_str() != "ultracode"
                    && !fields
                        .prompt_injected_values
                        .as_ref()
                        .and_then(Option::as_ref)
                        .is_some_and(|a| a.contains(&o.id))
            })
            .cloned()
            .collect(),
        _ => vec![],
    }
}
