//! New-thread defaults and destination-local model catalogs.
use crate::models::{self, CustomModel, ModelOption};
use t3_contracts::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    Web,
    Mobile,
}
#[derive(Debug, Clone, PartialEq)]
pub struct InstanceModelOption {
    pub instance_id: ProviderInstanceId,
    pub driver: ProviderDriverKind,
    pub provider_label: String,
    pub model: ModelOption,
}
impl InstanceModelOption {
    pub fn selection(&self) -> ModelSelection {
        ModelSelection {
            instance_id: self.instance_id.clone(),
            model: TrimmedNonEmptyString::new(&self.model.slug).expect("validated catalog slug"),
            options: None,
        }
    }
}
fn legacy_enabled(settings: &ServerSettings, driver: &str) -> Option<bool> {
    Some(match driver {
        "codex" => settings.providers.codex.enabled,
        "claudeAgent" => settings.providers.claude_agent.enabled,
        "cursor" => settings.providers.cursor.enabled,
        "grok" => settings.providers.grok.enabled,
        "pi" => settings.providers.pi.enabled,
        "opencode" => settings.providers.opencode.enabled,
        "antigravity" => settings.providers.antigravity.enabled,
        _ => return None,
    })
}
pub fn selection_provider_enabled(settings: &ServerSettings, selection: &ModelSelection) -> bool {
    settings
        .provider_instances
        .get(&selection.instance_id)
        .map(resolve_provider_instance_enabled)
        .unwrap_or_else(|| {
            legacy_enabled(settings, selection.instance_id.as_str()).unwrap_or(false)
        })
}
pub fn instance_enabled(settings: &ServerSettings, provider: &ServerProvider) -> bool {
    settings
        .provider_instances
        .get(&provider.instance_id)
        .map(resolve_provider_instance_enabled)
        .unwrap_or_else(|| {
            provider.instance_id.as_str() == provider.driver.as_str()
                && legacy_enabled(settings, provider.driver.as_str()).unwrap_or(provider.enabled)
        })
}
pub fn instance_custom_models(
    settings: &ServerSettings,
    provider: &ServerProvider,
) -> Vec<CustomModel> {
    if provider.driver.as_str() == "antigravity" {
        return vec![];
    }
    if let Some(value) = settings
        .provider_instances
        .get(&provider.instance_id)
        .and_then(|i| i.config.as_ref())
        .and_then(|c| c.get("customModels"))
        .filter(|v| v.is_array())
    {
        return models::read_custom_models(value);
    }
    if provider.instance_id.as_str() != provider.driver.as_str() {
        return vec![];
    }
    // Read only the authored model list. Never serialize provider environment secrets.
    let entries = match provider.driver.as_str() {
        "codex" => &settings.providers.codex.custom_models,
        "claudeAgent" => &settings.providers.claude_agent.custom_models,
        "cursor" => &settings.providers.cursor.custom_models,
        "grok" => &settings.providers.grok.custom_models,
        "pi" => &settings.providers.pi.custom_models,
        "opencode" => &settings.providers.opencode.custom_models,
        _ => return vec![],
    };
    models::read_custom_models(&serde_json::to_value(entries).expect("model settings serialize"))
}
pub fn instance_catalog(
    settings: &ServerSettings,
    client: &ClientSettings,
    provider: &ServerProvider,
    selected: Option<&str>,
) -> Vec<ModelOption> {
    let preferences = client.provider_model_preferences.get(&provider.instance_id);
    models::build_catalog(
        provider.driver.as_str(),
        &provider.models,
        &instance_custom_models(settings, provider),
        preferences
            .map(|p| p.hidden_models.as_slice())
            .unwrap_or_default(),
        preferences
            .map(|p| p.model_order.as_slice())
            .unwrap_or_default(),
        selected,
    )
}
pub fn available_models(
    config: &ServerConfig,
    client: &ClientSettings,
    surface: Surface,
    selected: Option<&ModelSelection>,
) -> Vec<InstanceModelOption> {
    let mut rows: Vec<_> = config
        .providers
        .0
        .iter()
        .filter(|p| {
            let enabled = instance_enabled(&config.settings, p);
            let available =
                p.availability.flatten() != Some(ServerProviderAvailability::Unavailable);
            match surface {
                Surface::Web => enabled && available && p.status == ServerProviderState::Ready,
                Surface::Mobile => {
                    enabled
                        && p.installed
                        && p.auth.status != ServerProviderAuthStatus::Unauthenticated
                        && (p.driver.as_str() != "antigravity" || available)
                }
            }
        })
        .flat_map(|p| {
            let label = p
                .display_name
                .as_ref()
                .and_then(Option::as_ref)
                .map(ToString::to_string)
                .unwrap_or_else(|| match p.driver.as_str() {
                    "codex" => "Codex".into(),
                    "claudeAgent" => "Claude".into(),
                    "pi" => "Pi".into(),
                    _ => p.instance_id.to_string(),
                });
            let models = match surface {
                Surface::Web => instance_catalog(
                    &config.settings,
                    client,
                    p,
                    selected
                        .filter(|s| s.instance_id == p.instance_id)
                        .map(|s| s.model.as_str()),
                ),
                Surface::Mobile => p.models.iter().map(ModelOption::from).collect(),
            };
            models.into_iter().map(move |model| InstanceModelOption {
                instance_id: p.instance_id.clone(),
                driver: p.driver.clone(),
                provider_label: label.clone(),
                model,
            })
        })
        .collect();
    if surface == Surface::Web {
        rows.sort_by_key(|row| {
            !client.favorites.iter().any(|favorite| {
                favorite.provider == row.instance_id && favorite.model.as_str() == row.model.slug
            })
        });
    }
    rows
}
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectDefaults {
    pub model_selection: Option<ModelSelection>,
    pub runtime_mode: RuntimeMode,
    pub environment_mode: Option<ThreadEnvMode>,
}
pub fn project_defaults(
    settings: &ServerSettings,
    project: Option<&ProjectShell>,
) -> ProjectDefaults {
    let stored = project.and_then(|p| settings.project_settings_overrides.get(&p.id));
    let legacy_model = (!settings.project_settings_folded)
        .then(|| project.and_then(|p| p.default_model_selection.as_ref()))
        .flatten();
    let candidate = stored
        .and_then(|p| p.default_model_selection.as_ref())
        .cloned()
        .or_else(|| legacy_model.cloned().map(Some));
    let model_selection = match candidate {
        Some(Some(selection)) if selection_provider_enabled(settings, &selection) => {
            Some(selection)
        }
        Some(None) => None,
        _ => settings.default_model_selection.clone(),
    };
    let legacy_environment = (!settings.project_settings_folded)
        .then(|| {
            project
                .and_then(|p| p.extra.get("defaultThreadEnvMode"))
                .and_then(|v| serde_json::from_value(v.clone()).ok())
        })
        .flatten();
    ProjectDefaults {
        model_selection,
        runtime_mode: stored
            .and_then(|p| p.default_runtime_mode)
            .unwrap_or(settings.default_runtime_mode),
        environment_mode: stored
            .and_then(|p| p.default_thread_env_mode)
            .or(legacy_environment)
            .or(settings.default_thread_env_mode),
    }
}
/// Resolve draft > project > sticky > declared default > first usable row.
/// Web retains provider-owned stored identifiers; mobile rejects implicit legacy
/// defaults while deliberately retaining known Antigravity selections.
pub fn resolve_new_thread_selection(
    config: &ServerConfig,
    client: &ClientSettings,
    surface: Surface,
    draft: Option<&ModelSelection>,
    defaults: &ProjectDefaults,
    sticky: Option<&ModelSelection>,
) -> Option<ModelSelection> {
    if surface == Surface::Web {
        // Picker readiness is stricter than persisted selection validity. A
        // requested instance can recover from a probe error without losing its
        // provider-owned unknown model identifier or explicit options.
        let selected = draft.or(defaults.model_selection.as_ref()).or(sticky);
        let selectable = |p: &&ServerProvider| {
            instance_enabled(&config.settings, p)
                && p.availability.flatten() != Some(ServerProviderAvailability::Unavailable)
        };
        let provider = selected
            .and_then(|s| {
                config
                    .providers
                    .0
                    .iter()
                    .find(|p| p.instance_id == s.instance_id && selectable(p))
            })
            .or_else(|| {
                config
                    .providers
                    .0
                    .iter()
                    .filter(selectable)
                    .find(|p| p.status == ServerProviderState::Ready)
            })
            .or_else(|| {
                config
                    .providers
                    .0
                    .iter()
                    .filter(selectable)
                    .find(|p| p.status != ServerProviderState::Error)
            })?;
        return selection_for_provider(provider, selected);
    }
    for (candidate, explicit) in [
        (draft, true),
        (defaults.model_selection.as_ref(), false),
        (sticky, false),
    ] {
        let Some(candidate) = candidate else {
            continue;
        };
        let provider = config
            .providers
            .0
            .iter()
            .find(|p| p.instance_id == candidate.instance_id);
        let driver = provider.map(|p| p.driver.as_str()).or_else(|| {
            config
                .settings
                .provider_instances
                .get(&candidate.instance_id)
                .map(|p| p.driver.as_str())
        });
        if driver == Some("antigravity") {
            return Some(candidate.clone());
        }
        let Some(provider) = provider else {
            continue;
        };
        if instance_enabled(&config.settings, provider)
            && provider.installed
            && provider.auth.status != ServerProviderAuthStatus::Unauthenticated
        {
            if !explicit
                && provider
                    .models
                    .iter()
                    .any(|m| m.slug == candidate.model && m.is_legacy.flatten() == Some(true))
            {
                continue;
            }
            return Some(candidate.clone());
        }
    }
    let rows = available_models(config, client, surface, None);
    let eligible = |row: &&InstanceModelOption| !row.model.is_unavailable && !row.model.is_legacy;
    rows.iter()
        .filter(eligible)
        .find(|row| row.model.is_default)
        .or_else(|| rows.iter().filter(eligible).next())
        .map(InstanceModelOption::selection)
}

pub fn resolve_selectable_provider_instance<'a>(
    providers: &'a [ServerProvider],
    requested: Option<&ProviderInstanceId>,
) -> Option<&'a ServerProvider> {
    let selectable = |p: &&ServerProvider| {
        p.enabled && p.availability.flatten() != Some(ServerProviderAvailability::Unavailable)
    };
    requested
        .and_then(|id| {
            providers
                .iter()
                .find(|p| &p.instance_id == id && selectable(p))
        })
        .or_else(|| {
            providers
                .iter()
                .filter(selectable)
                .find(|p| p.status == ServerProviderState::Ready)
        })
        .or_else(|| {
            providers
                .iter()
                .filter(selectable)
                .find(|p| p.status != ServerProviderState::Error)
        })
}
fn default_driver_model(driver: &str) -> Option<&'static str> {
    match driver {
        "codex" => Some("gpt-6-astra"),
        "claudeAgent" => Some("claude-fable-5-1"),
        "cursor" => Some("auto"),
        "grok" => Some("grok-build"),
        "acpRegistry" | "pi" => Some("default"),
        "opencode" => Some("openai/gpt-5"),
        "antigravity" => Some(models::ANTIGRAVITY_DEFAULT_MODEL),
        _ => None,
    }
}
pub fn resolve_default_provider_model_selection(
    providers: &[ServerProvider],
    selected: Option<&ModelSelection>,
) -> Option<ModelSelection> {
    let provider =
        resolve_selectable_provider_instance(providers, selected.map(|s| &s.instance_id))?;
    selection_for_provider(provider, selected)
}
fn selection_for_provider(
    provider: &ServerProvider,
    selected: Option<&ModelSelection>,
) -> Option<ModelSelection> {
    if let Some(selected) = selected.filter(|s| s.instance_id == provider.instance_id) {
        return Some(selected.clone());
    }
    let model = provider
        .models
        .iter()
        .find(|m| !m.is_custom && m.is_default.flatten() == Some(true))
        .or_else(|| provider.models.iter().find(|m| !m.is_custom))
        .or_else(|| provider.models.first())
        .map(|m| m.slug.as_str())
        .or_else(|| default_driver_model(provider.driver.as_str()))?;
    Some(ModelSelection {
        instance_id: provider.instance_id.clone(),
        model: TrimmedNonEmptyString::new(model).expect("validated model slug"),
        options: None,
    })
}

pub fn launch_selection(
    config: &ServerConfig,
    client: &ClientSettings,
    selection: &ModelSelection,
    mode: RuntimeMode,
) -> Option<(ModelSelection, RuntimeMode)> {
    let provider = config
        .providers
        .0
        .iter()
        .find(|p| p.instance_id == selection.instance_id)?;
    let mut selection = selection.clone();
    selection.options = models::composer_dispatch_options(
        provider.driver.as_str(),
        &provider.models,
        selection.model.as_str(),
        selection.options.as_deref(),
        client.plan_mode_enabled,
    );
    let supported = provider
        .supported_runtime_modes
        .as_ref()
        .and_then(Option::as_ref)
        .map(|s| s.0.as_slice());
    Some((selection, models::compatible_runtime_mode(mode, supported)))
}
