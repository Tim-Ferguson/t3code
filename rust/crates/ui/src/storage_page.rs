//! The Storage settings page (`/settings/storage`): where worktrees go, when a worktree is deleted, and how long browser
//! artifacts and rotated logs are kept. Port of `StorageSettings.tsx`. The cleanup rules are the environment's
//! (`storageCleanup`), and a project may inherit them, turn them off or carry rules of its own (`worktreeCleanup` in
//! the project's settings override, capability `projectWorktreeCleanup`). Every change is one `server.updateSettings`
//! patch that needs the settings grant.
use crate::{
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt, View},
    settings_common::{Tabs, connected_allows},
};
use dioxus::prelude::*;
use serde_json::{Value, json};
use t3_client::lifecycle as rules;
use t3_contracts::AuthEnvironmentScope;

/// The four rules of one project's custom policy, with the change applied over the rules in force now.
fn custom_rules(settings: &Value, project: &str, key: &str, value: Value) -> Value {
    let mut current = rules::worktree_cleanup_rules(settings, Some(project));
    if !current.is_object() {
        current = rules::worktree_cleanup_rules(settings, None);
    }
    current[key] = value;
    current
}

/// The patch that sets a project's own cleanup policy, replacing whatever it held (an override stores the whole value).
fn project_policy_patch(settings: &Value, project: &str, policy: Option<Value>) -> Value {
    let mut entry = settings["projectSettingsOverrides"][project].as_object().cloned().unwrap_or_default();
    match policy {
        Some(policy) => {
            entry.insert("worktreeCleanup".into(), policy);
        }
        None => {
            entry.remove("worktreeCleanup");
        }
    }
    json!({"patch": {"projectSettingsOverrides": {project: if entry.is_empty() { Value::Null } else { Value::Object(entry) }}}})
}

#[component]
pub(crate) fn Page(state: Store<UiModel>, transport: TransportHandle) -> Element {
    let mut scope = use_signal(String::new);
    let mut saving = use_signal(|| false);
    let mut failure = use_signal(|| None::<String>);
    let settings_write = use_memo(move || connected_allows(state, AuthEnvironmentScope::SettingsWrite));
    let readable = use_memo(move || connected_allows(state, AuthEnvironmentScope::OrchestrationRead));
    let config = state.config().read().clone();
    let settings = config["settings"].clone();
    let capabilities = config["environment"]["capabilities"].clone();
    let projects: Vec<(String, String)> = state.shell().read().snapshot.as_ref().map(|snapshot| snapshot.projects.iter().map(|project| (project.id.to_string(), project.title.to_string())).collect()).unwrap_or_default();
    let project_cleanup = capabilities["projectWorktreeCleanup"] == true && capabilities["projectSettingsOverrides"] == true;
    let project = Some(scope.read().clone()).filter(|id| !id.is_empty() && project_cleanup && projects.iter().any(|(project, _)| project == id));
    let writable = settings_write() && !*saving.read() && config.is_object() && settings.is_object();

    let save = {
        let transport = transport.clone();
        use_callback(move |patch: Value| {
            if *saving.peek() || !*settings_write.peek() {
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
    // Environment rules go to `storageCleanup`; a project's custom rules go to its own policy.
    let update_rule = {
        let (settings, project) = (settings.clone(), project.clone());
        move |key: &'static str, value: Value| match project.as_deref() {
            Some(project) => save.call(project_policy_patch(&settings, project, Some(json!({"mode": "custom", "rules": custom_rules(&settings, project, key, value)})))),
            None => save.call(json!({"patch": {"storageCleanup": {key: value}}})),
        }
    };
    let update_environment = move |key: &'static str, value: Value| save.call(json!({"patch": {key: value}}));
    let choose_mode = {
        let settings = settings.clone();
        move |project: String, mode: String| {
            let policy = match mode.as_str() {
                "off" => Some(json!({"mode": "off"})),
                "custom" => Some(json!({"mode": "custom", "rules": rules::worktree_cleanup_rules(&settings, None)})),
                _ => None,
            };
            save.call(project_policy_patch(&settings, &project, policy));
        }
    };

    let in_force = rules::worktree_cleanup_rules(&settings, project.as_deref());
    let mode = project.as_deref().map(|project| rules::project_cleanup_mode(&settings, project));
    let cleanup = settings["storageCleanup"].clone();
    let rules_shown = project.is_none() || mode == Some("custom");
    let storage_supported = capabilities["storageCleanup"] == true;

    let retention = |key: &'static str, title: &'static str, description: &'static str, current: Option<u64>| -> Element {
        let update = update_rule.clone();
        rsx! { div { key: "{key}", class: "setting-row", "data-setting": "{key}",
            div { class: "font-description", strong { "{title}" } p { class: "muted", "{description}" } }
            div { class: "font-controls",
                input { r#type: "checkbox", role: "switch", "aria-label": "{title}", checked: current.is_some(), disabled: !writable,
                    onchange: { let update = update.clone(); move |event| { let value = if event.checked() { Value::from(8) } else { Value::Null }; update(key, value) } } }
                if let Some(current) = current {
                    input { r#type: "number", min: "1", max: "3650", "aria-label": "{title} in days", value: "{current}", disabled: !writable,
                        onchange: { let update = update.clone(); move |event| { if let Ok(days) = event.value().parse::<u64>() { let value = Value::from(days.clamp(1, 3650)); update(key, value) } } } }
                    " days"
                }
            }
        } }
    };
    let flag = |key: &'static str, title: &'static str, description: &'static str| -> Element {
        let on = in_force[key].as_bool().unwrap_or(false);
        let update = update_rule.clone();
        rsx! { div { key: "{key}", class: "setting-row", "data-setting": "{key}",
            div { class: "font-description", strong { "{title}" } p { class: "muted", "{description}" } }
            div { class: "font-controls", input { r#type: "checkbox", role: "switch", "aria-label": "{title}", checked: on, disabled: !writable, onchange: move |event| update(key, Value::Bool(event.checked())) } }
        } }
    };
    let location = settings["worktreesDirectory"].as_str().unwrap_or_default().to_owned();

    rsx! {
        section { class: "settings-page", Tabs { state, current: View::Storage }
            div { class: "page-heading", h1 { "Storage" } }
            p { class: "muted", "Where worktrees go, and when old worktrees, browser captures and logs are deleted." }
            if !settings_write() { p { class: "muted", role: "status", "This connection can view these settings but cannot change them." } }
            if let Some(message) = failure.read().clone() { p { class: "error-banner", role: "alert", "{message}" } }
            if *saving.read() { p { role: "status", "Saving settings\u{2026}" } }
            if !readable() || !config.is_object() || !settings.is_object() {
                p { class: "muted", role: "status", "Connect an environment to choose its settings." }
            } else if !storage_supported {
                p { class: "muted", role: "status", "Update the environment to use storage cleanup." }
            } else {
                if project_cleanup {
                    div { class: "field-row",
                        label { r#for: "storage-scope", "Settings for" }
                        select { id: "storage-scope", "aria-label": "Storage settings scope", value: "{scope}", disabled: *saving.read(), onchange: move |event| scope.set(event.value()),
                            option { value: "", selected: scope.read().is_empty(), "Environment defaults" }
                            for (id, title) in projects.iter() { option { key: "{id}", value: "{id}", selected: *scope.read() == *id, "Project: {title}" } }
                        }
                    }
                }
                section { class: "sc-section", id: "storage-worktrees", "aria-label": "Worktrees", h2 { "Worktrees" }
                    if project.is_none() {
                        div { class: "setting-row", id: "storage-worktrees-location", "data-setting": "worktreesDirectory",
                            div { class: "font-description", strong { "Worktree location" } p { class: "muted", "Folder where new worktrees are created, such as ~/worktrees. Existing worktrees stay where they are. Leave empty to use the default." } }
                            div { class: "font-controls", input { "aria-label": "Worktree location", placeholder: "Default", value: "{location}", disabled: !writable, autocapitalize: "none", spellcheck: "false",
                                onchange: move |event| update_environment("worktreesDirectory", Value::String(event.value().trim().to_owned())) } }
                        }
                    }
                    if let (Some(project_id), Some(mode)) = (project.clone(), mode) {
                        div { class: "setting-row", "data-setting": "worktreeCleanup",
                            div { class: "font-description", strong { "Automatic worktree cleanup" }
                                p { class: "muted", {match mode {
                                    "off" => "Keep this project's worktrees until you delete them manually.",
                                    "custom" => "Use these rules for this project.",
                                    _ => "Use each machine's worktree cleanup settings.",
                                }} }
                            }
                            div { class: "font-controls",
                                select { "aria-label": "Automatic worktree cleanup", value: "{mode}", disabled: !writable, onchange: move |event| choose_mode(project_id.clone(), event.value()),
                                    option { value: "inherit", selected: mode == "inherit", "Inherit" }
                                    option { value: "off", selected: mode == "off", "Off" }
                                    option { value: "custom", selected: mode == "custom", "Custom" }
                                }
                            }
                        }
                    }
                    if rules_shown {
                        {flag("worktreeOnDelete", "Delete worktrees with deleted threads", "Remove unused worktrees when active or archived threads are deleted. Worktrees with local changes are kept.")}
                        {retention("worktreeAfterDays", "Delete inactive worktrees", "Remove worktrees after their threads have been inactive for this many days. Branches and thread history are kept.", in_force["worktreeAfterDays"].as_u64())}
                        {flag("worktreeOnMerge", "Delete merged worktrees", "Remove worktrees whose pull request is merged and whose commits are included in the default branch.")}
                        {flag("worktreeUnchanged", "Delete unchanged worktrees", "Remove worktrees with no commits beyond the default branch.")}
                    }
                }
                if project.is_none() {
                    section { class: "sc-section", id: "storage-artifacts", "aria-label": "Artifacts and logs", h2 { "Artifacts and logs" }
                        {retention("browserArtifactsAfterDays", "Delete old browser artifacts", "Delete saved browser captures after this many days. Older capture links will no longer open.", cleanup["browserArtifactsAfterDays"].as_u64())}
                        {retention("logsAfterDays", "Delete old rotated logs", "Delete inactive rotated log files after this many days. Current logs are kept.", cleanup["logsAfterDays"].as_u64())}
                    }
                }
            }
        }
    }
}
