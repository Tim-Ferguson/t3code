//! The settings a project may override, as rows that edit the environment's defaults or, with a
//! project chosen, that project's overrides: default model, permissions, where new threads start,
//! submodules, pulling, agent credits and the merge method. Port of `ProjectDefaultsSettings.tsx`.
//! One `server.updateSettings` patch per change; with a project the patch replaces that project's
//! whole override entry, and "Inherit" sends the entry without the key.
use crate::{
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
    settings_common::connected_allows,
};
use dioxus::prelude::*;
use serde_json::{Value, json};
use t3_client::text_settings as logic;
use t3_contracts::AuthEnvironmentScope;

/// One overridable setting: its key, wording, and the options a person picks from (JSON text, label).
struct Scoped {
    key: &'static str,
    title: &'static str,
    description: &'static str,
    /// The value shown while the server's settings do not name one.
    fallback: &'static str,
    options: &'static [(&'static str, &'static str)],
}
const PERMISSIONS: &[(&str, &str)] = &[("approval-required", "Supervised"), ("auto-accept-edits", "Auto-accept edits"), ("auto", "Auto"), ("full-access", "Full access")];
const ROWS: [Scoped; 4] = [
    Scoped { key: "defaultRuntimeMode", title: "Permissions", description: "Default permissions for new threads. Projects can override them.", fallback: "full-access", options: PERMISSIONS },
    Scoped { key: "defaultThreadEnvMode", title: "New threads", description: "Where new threads start. Projects and their t3.json can override it.", fallback: "local", options: &[("local", "Current checkout"), ("worktree", "New worktree")] },
    Scoped { key: "worktreeSubmodules", title: "Submodules", description: "How a new worktree gets the repository's submodules.", fallback: "recursive", options: &[("recursive", "Recursive"), ("top-level", "Top level only"), ("none", "Skip")] },
    Scoped { key: "pullRequestMergeMethod", title: "Default merge method", description: "The method the merge button uses first. Last used remembers the previous choice.", fallback: "last", options: &[("last", "Last used"), ("merge", "Merge"), ("squash", "Squash and merge"), ("rebase", "Rebase and merge")] },
];
const SWITCHES: [(&str, &str, &str, bool); 3] = [
    ("newWorktreesStartFromOrigin", "Start from origin", "Creates the worktree from the latest matching branch on origin instead of your local branch.", true),
    ("defaultAutoPull", "Automatically pull", "Pull a thread's branch from its remote when the thread opens.", false),
    ("removeAgentCreditsOnMerge", "Remove agent credits when merging", "Strip the agent's co-author lines and credit footers from the commit when a pull request is merged.", false),
];

/// The wording of a row for one project (`isProjectScope`): the two that name their scope say "this project".
fn project_words(key: &str, environment: &'static str, project: bool) -> &'static str {
    match (project, key) {
        (true, "defaultRuntimeMode") => "Permissions for new threads in this project.",
        (true, "defaultThreadEnvMode") => "Where new threads in this project start.",
        _ => environment,
    }
}

/// The rows, for the environment (`project` None) or one project's overrides.
#[component]
pub(crate) fn ScopedRows(state: Store<UiModel>, transport: TransportHandle, project: Option<String>) -> Element {
    let mut saving = use_signal(|| false);
    let mut failure = use_signal(|| None::<String>);
    let write_ok = use_memo(move || connected_allows(state, AuthEnvironmentScope::SettingsWrite));
    let config = state.config().read().clone();
    let settings = config["settings"].clone();
    let writable = write_ok() && !*saving.read() && settings.is_object();
    let save = {
        let transport = transport.clone();
        use_callback(move |patch: Value| {
            if *saving.peek() || !*write_ok.peek() {
                return;
            }
            let Some(owner) = runtime::connection_owner(&transport, state) else { return };
            saving.set(true);
            failure.set(None);
            let transport = transport.clone();
            spawn(async move {
                let mut result = runtime::request_value_reported(transport.clone(), state, "server.updateSettings", patch).await.map(|_| ());
                if result.is_ok() && runtime::connection_owner(&transport, state).as_ref() == Some(&owner) && !runtime::live_config_active(&transport, state) {
                    result = runtime::request_value_reported(transport.clone(), state, "server.getConfig", json!({})).await.map(|_| ());
                }
                saving.set(false);
                if runtime::connection_owner(&transport, state).as_ref() != Some(&owner) {
                    return;
                }
                if let Err(message) = result {
                    failure.set(Some(format!("Could not save settings. {message}")));
                }
            });
        })
    };
    let scope = project.clone();
    let set = {
        let (settings, scope) = (settings.clone(), scope.clone());
        move |key: &str, value: Value| save.call(logic::set_patch(&settings, scope.as_deref(), key, value))
    };
    let inherit_hint = |key: &'static str| -> Element {
        match scope.as_deref() {
            Some(_) if logic::is_overridden(&settings, scope.as_deref(), key) => {
                let (settings, scope) = (settings.clone(), scope.clone());
                rsx! { span { class: "muted", "Overridden for this project. " } button { r#type: "button", "aria-label": "Use the environment value for {key}", disabled: !writable,
                    onclick: move |_| { if let Some(patch) = logic::clear_patch(&settings, scope.as_deref(), key) { save.call(patch); } }, "Inherit" } }
            }
            Some(_) => rsx! { span { class: "muted", "Inherited from the environment." } },
            None => rsx! {},
        }
    };
    // The models a new thread can use, for the default-model row.
    let models: Vec<(String, String, String)> = state.typed_config().read().as_ref().map(|config| {
        t3_client::new_thread::available_models(config, &state.client_settings().read(), runtime::ui_surface(), None).into_iter().map(|row| (row.instance_id.to_string(), row.model.slug.clone(), format!("{} \u{b7} {}", row.provider_label, row.model.name))).collect()
    }).unwrap_or_default();
    let model = logic::effective(&settings, scope.as_deref(), "defaultModelSelection");
    let model_value = match (model["instanceId"].as_str(), model["model"].as_str()) {
        (Some(instance), Some(slug)) => format!("{instance}|{slug}"),
        _ => String::new(),
    };
    rsx! {
        section { class: "sc-section", "aria-label": "New threads", "data-scope": if scope.is_some() { "project" } else { "environment" },
            h2 { "New threads" }
            if let Some(message) = failure.read().clone() { p { class: "error-banner", role: "alert", "{message}" } }
            div { class: "setting-row", "data-setting": "defaultModelSelection", id: "default-model",
                div { class: "font-description", strong { "Default model" } p { class: "muted", "The model for new threads. " {inherit_hint("defaultModelSelection")} } }
                div { class: "font-controls",
                    select { "aria-label": "Default model", disabled: !writable, value: "{model_value}",
                        onchange: { let set = set.clone(); move |event| {
                            let picked = event.value();
                            let value = match picked.split_once('|') { Some((instance, slug)) => json!({"instanceId":instance,"model":slug}), None => Value::Null };
                            set("defaultModelSelection", value);
                        } },
                        option { value: "", selected: model_value.is_empty(), "No default" }
                        for (instance, slug, label) in models.iter() { option { key: "{instance}|{slug}", value: "{instance}|{slug}", selected: model_value == format!("{instance}|{slug}"), "{label}" } }
                    }
                }
            }
            for row in ROWS.iter() {
                {
                    let current = logic::effective(&settings, scope.as_deref(), row.key);
                    let shown = current.as_str().unwrap_or(row.fallback).to_owned();
                    let key = row.key;
                    let set = set.clone();
                    rsx! { div { key: "{row.key}", class: "setting-row", "data-setting": "{row.key}", id: match row.key { "defaultRuntimeMode" => Some("default-permissions"), "defaultThreadEnvMode" => Some("new-threads"), _ => None },
                        div { class: "font-description", strong { "{row.title}" } p { class: "muted", "{project_words(row.key, row.description, scope.is_some())} " {inherit_hint(row.key)} } }
                        div { class: "font-controls",
                            select { "aria-label": "{row.title}", disabled: !writable, value: "{shown}",
                                onchange: move |event| {
                                    let picked = event.value();
                                    // "Last used" is the absence of a method.
                                    set(key, if key == "pullRequestMergeMethod" && picked == "last" { Value::Null } else { Value::String(picked) });
                                },
                                for (value, label) in row.options.iter() { option { key: "{value}", value: "{value}", selected: shown == *value, "{label}" } }
                            }
                        }
                    } }
                }
            }
            for (key, title, description, default) in SWITCHES {
                {
                    let checked = logic::effective(&settings, scope.as_deref(), key).as_bool().unwrap_or(default);
                    let set = set.clone();
                    rsx! { div { key: "{key}", class: "setting-row", "data-setting": "{key}",
                        div { class: "font-description", strong { "{title}" } p { class: "muted", "{description} " {inherit_hint(key)} } }
                        div { class: "font-controls", input { r#type: "checkbox", role: "switch", "aria-label": "{title}", checked, disabled: !writable, onchange: move |event| set(key, Value::Bool(event.checked())) } }
                    } }
                }
            }
        }
    }
}
