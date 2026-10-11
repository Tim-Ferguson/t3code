//! The General settings page: what happens to threads on their own. A thread that stopped on a usage
//! limit can resume or sleep until the reset, and a thread nobody touched, or whose pull request
//! merged, settles. Port of the "Organization" rows of `SettingsPanels.tsx` that the server acts on:
//! `autoResumeLimitedThreads`, `snoozeLimitedThreads`, `sidebarAutoSettleOnMerge` and
//! `sidebarAutoSettleAfterDays`. The two settle settings can be overridden for one project.
//!
//! Every change is one `server.updateSettings` patch, needs the settings grant, and is sent once:
//! the controls wait for the server's answer and show what the server's configuration says.
use crate::{
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt, View},
    settings_common::{Tabs, connected_allows},
    settings_rows::{self, ClientRows},
    project_defaults::ScopedRows,
};
use dioxus::prelude::*;
use serde_json::Value;
use t3_client::text_settings as logic;
use t3_contracts::AuthEnvironmentScope;

#[component]
pub(crate) fn Page(state: Store<UiModel>, transport: TransportHandle) -> Element {
    let mut scope = use_signal(String::new);
    let mut saving = use_signal(|| false);
    let mut failure = use_signal(|| None::<String>);
    let mut days_draft = use_signal(|| None::<String>);
    let settings_write = use_memo(move || connected_allows(state, AuthEnvironmentScope::SettingsWrite));
    let readable = use_memo(move || connected_allows(state, AuthEnvironmentScope::OrchestrationRead));
    let config = state.config().read().clone();
    let settings = config["settings"].clone();
    let capabilities = config["environment"]["capabilities"].clone();
    let overrides_supported = capabilities["projectSettingsOverrides"] == true;
    let auto_settlement = capabilities["threadAutoSettlement"] == true;
    let projects: Vec<(String, String)> = state.shell().read().snapshot.as_ref().map(|snapshot| snapshot.projects.iter().map(|project| (project.id.to_string(), project.title.to_string())).collect()).unwrap_or_default();
    let project = Some(scope.read().clone()).filter(|id| !id.is_empty() && overrides_supported && projects.iter().any(|(project, _)| project == id));
    let writable = settings_write() && !*saving.read() && config.is_object() && config["settings"].is_object();

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
                    result = runtime::request_value_reported(transport.clone(), state, "server.getConfig", serde_json::json!({})).await.map(|_| ());
                }
                saving.set(false);
                if runtime::connection_owner(&transport, state).as_ref() != Some(&owner) {
                    return;
                }
                days_draft.set(None);
                if let Err(message) = result {
                    failure.set(Some(format!("Could not save settings. {message}")));
                }
            });
        })
    };
    let set = {
        let (settings, project) = (settings.clone(), project.clone());
        move |key: &str, value: Value| save.call(logic::set_patch(&settings, project.as_deref(), key, value))
    };
    let inherit = {
        let (settings, project) = (settings.clone(), project.clone());
        move |key: &str| {
            if let Some(patch) = logic::clear_patch(&settings, project.as_deref(), key) {
                save.call(patch);
            }
        }
    };
    let source = |key: &str| -> Element {
        match project.as_deref() {
            Some(_) if logic::is_overridden(&settings, project.as_deref(), key) => {
                let inherit = inherit.clone();
                let owned = key.to_owned();
                rsx! { span { class: "muted", "Overridden for this project. " } button { r#type: "button", "aria-label": "Use the environment value for {key}", disabled: !writable, onclick: move |_| inherit(&owned), "Inherit" } }
            }
            Some(_) => rsx! { span { class: "muted", "Inherited from the environment." } },
            None => rsx! {},
        }
    };
    let env_value = |row: settings_rows::Row| match &settings[row.key] {
        Value::Null => serde_json::from_str(row.default).unwrap_or(Value::Null),
        value => value.clone(),
    };
    let flag = |key: &str| logic::effective(&settings, project.as_deref(), key) == true;
    let days = logic::settle_days(&logic::effective(&settings, project.as_deref(), logic::SETTLE_AFTER_DAYS));

    rsx! {
        section { class: "settings-page", Tabs { state, current: View::General }
            div { class: "page-heading",
                h1 { "General" }
                RestoreDefaults { state }
            }
            p { class: "muted", "How threads, the composer and diffs behave." }
            if !settings_write() { p { class: "muted", role: "status", "This connection can view these settings but cannot change them." } }
            if let Some(message) = failure.read().clone() { p { class: "error-banner", role: "alert", "{message}" } }
            if *saving.read() { p { role: "status", "Saving settings\u{2026}" } }
            if !readable() || !config.is_object() || !config["settings"].is_object() {
                p { class: "muted", role: "status", "Connect an environment to choose its settings." }
            } else {
                if overrides_supported {
                    div { class: "field-row",
                        label { r#for: "general-scope", "Settings for" }
                        select { id: "general-scope", "aria-label": "General settings scope", value: "{scope}", disabled: *saving.read(), onchange: move |event| scope.set(event.value()),
                            option { value: "", selected: scope.read().is_empty(), "Environment defaults" }
                            for (id, title) in projects.iter() { option { key: "{id}", value: "{id}", selected: *scope.read() == *id, "Project: {title}" } }
                        }
                    }
                }
                section { class: "sc-section", "aria-label": "Organization",
                    h2 { "Organization" }
                    if project.is_none() {
                        label { class: "switch-row",
                            input { r#type: "checkbox", role: "switch", "aria-label": "Auto-resume limited threads", checked: flag(logic::AUTO_RESUME_LIMITED), disabled: !writable,
                                onchange: { let set = set.clone(); move |event| set(logic::AUTO_RESUME_LIMITED, Value::Bool(event.checked())) } }
                            " Auto-resume limited threads" }
                        p { class: "muted", "Resume usage-limit stops at the reported reset time. Each thread can cancel its scheduled continuation." }
                        label { class: "switch-row",
                            input { r#type: "checkbox", role: "switch", "aria-label": "Snooze limited threads", checked: flag(logic::SNOOZE_LIMITED), disabled: !writable,
                                onchange: { let set = set.clone(); move |event| set(logic::SNOOZE_LIMITED, Value::Bool(event.checked())) } }
                            " Snooze limited threads" }
                        p { class: "muted", "Snooze usage-limit stops until the reported reset time. Combine with auto-resume to continue when they wake." }
                    }
                    if auto_settlement {
                        label { class: "switch-row",
                            input { r#type: "checkbox", role: "switch", "aria-label": "Auto-settle merged threads", checked: flag(logic::SETTLE_ON_MERGE), disabled: !writable,
                                onchange: { let set = set.clone(); move |event| set(logic::SETTLE_ON_MERGE, Value::Bool(event.checked())) } }
                            " Auto-settle merged threads" }
                        p { class: "muted", "Settle a thread when its pull request merges. Closed pull requests still settle automatically. " {source(logic::SETTLE_ON_MERGE)} }
                        label { class: "switch-row",
                            input { r#type: "checkbox", role: "switch", "aria-label": "Auto-settle inactive threads", checked: days.is_some(), disabled: !writable,
                                onchange: { let set = set.clone(); move |event| set(logic::SETTLE_AFTER_DAYS, if event.checked() { Value::from(logic::SETTLE_DEFAULT_DAYS) } else { Value::Null }) } }
                            " Auto-settle inactive threads" }
                        p { class: "muted", "Sidebar threads with no activity for this long settle automatically. " {source(logic::SETTLE_AFTER_DAYS)} }
                        if let Some(current) = days {
                            label { r#for: "settle-days", "Days before auto-settle" }
                            // A draft the field can be emptied in; the setting moves only on a valid count and
                            // the field returns to the saved one when it is left.
                            input { id: "settle-days", r#type: "number", min: "{logic::SETTLE_MIN_DAYS}", max: "{logic::SETTLE_MAX_DAYS}", "aria-label": "Days of inactivity before auto-settle",
                                value: "{days_draft.read().clone().unwrap_or_else(|| current.to_string())}", disabled: !writable,
                                oninput: { let set = set.clone(); move |event| {
                                    let typed = event.value();
                                    days_draft.set(Some(typed.clone()));
                                    if let Some(parsed) = logic::parse_settle_days(&typed).filter(|parsed| *parsed != current) { set(logic::SETTLE_AFTER_DAYS, Value::from(parsed)); }
                                } },
                                onblur: move |_| days_draft.set(None) }
                            p { class: "muted", "Any new activity un-settles a thread automatically." }
                        }
                    }
                }
                div { key: "scoped:{scope}", ScopedRows { state, transport: transport.clone(), project: project.clone() } }
                section { class: "sc-section", "aria-label": "Behavior",
                    h2 { "Behavior" }
                    crate::notifications::ModeRow { state }
                    ClientRows { state, rows: &settings_rows::BEHAVIOR }
                    // These belong to the environment, not to one project.
                    if project.is_none() {
                        for row in settings_rows::ENVIRONMENT {
                            { settings_rows::control(row, &env_value(row), !writable, { let set = set.clone(); EventHandler::new(move |value: Value| set(row.key, value)) }) }
                        }
                    }
                }
                section { class: "sc-section", "aria-label": "Confirmations", h2 { "Confirmations" } ClientRows { state, rows: &settings_rows::CONFIRMATIONS } }
                section { class: "sc-section", "aria-label": "Diffs", h2 { "Diffs" } ClientRows { state, rows: &settings_rows::DIFFS } }
                section { class: "sc-section", "aria-label": "Display", h2 { "Display" } ClientRows { state, rows: &settings_rows::DISPLAY } }
                if project.is_none() {
                    section { class: "sc-section", id: "storage", "aria-label": "Storage", h2 { "Storage" }
                        { storage_rows(&settings, writable, set.clone()) }
                    }
                }
            }
        }
    }
}

/// "Restore device defaults": every preference of this device back to its default, and the theme back to the system's.
#[component]
fn RestoreDefaults(state: Store<UiModel>) -> Element {
    let themes = try_consume_context::<crate::themes::Themes>();
    let themed = themes.as_ref().is_some_and(|themes| {
        let snapshot = themes.snapshot.read();
        snapshot.theme != "system" || snapshot.appearance_mode != t3_client::themes::Mode::System
    });
    let dirty = !settings_rows::changed_labels(state).is_empty() || themed;
    rsx! {
        button { r#type: "button", "aria-label": "Restore device defaults", disabled: !dirty,
            title: "Put this device's preferences, such as theme and layout, back to their defaults",
            onclick: move |_| {
                settings_rows::restore_defaults(state);
                if let Some(themes) = &themes {
                    themes.change(t3_client::themes::storage::Action::Theme("system".into()));
                    themes.change(t3_client::themes::storage::Action::Mode(t3_client::themes::Mode::System));
                }
            },
            "Restore device defaults" }
    }
}

/// The environment's storage settings: where worktrees go and when old files are cleaned up.
fn storage_rows(settings: &Value, writable: bool, set: impl Fn(&str, Value) + Clone + 'static) -> Element {
    let cleanup = settings["storageCleanup"].clone();
    let patch = {
        let set = set.clone();
        move |key: &'static str, value: Value| set("storageCleanup", serde_json::json!({ key: value }))
    };
    let days = |key: &str| cleanup[key].as_u64();
    let retention = |key: &'static str, title: &'static str, description: &'static str| -> Element {
        let current = days(key);
        let patch = patch.clone();
        rsx! { div { key: "{key}", class: "setting-row", "data-setting": "{key}",
            div { class: "font-description", strong { "{title}" } p { class: "muted", "{description}" } }
            div { class: "font-controls",
                input { r#type: "checkbox", role: "switch", "aria-label": "{title}", checked: current.is_some(), disabled: !writable, onchange: { let patch = patch.clone(); move |event| patch(key, if event.checked() { Value::from(8) } else { Value::Null }) } }
                if let Some(current) = current {
                    input { r#type: "number", min: "1", max: "3650", "aria-label": "{title} in days", value: "{current}", disabled: !writable,
                        onchange: { let patch = patch.clone(); move |event| { if let Ok(days) = event.value().parse::<u64>() { patch(key, Value::from(days.clamp(1, 3650))); } } } }
                    " days"
                }
            }
        } }
    };
    let flag = |key: &'static str, title: &'static str, description: &'static str| -> Element {
        let on = cleanup[key].as_bool().unwrap_or(false);
        let patch = patch.clone();
        rsx! { div { key: "{key}", class: "setting-row", "data-setting": "{key}",
            div { class: "font-description", strong { "{title}" } p { class: "muted", "{description}" } }
            div { class: "font-controls", input { r#type: "checkbox", role: "switch", "aria-label": "{title}", checked: on, disabled: !writable, onchange: move |event| patch(key, Value::Bool(event.checked())) } }
        } }
    };
    let location = settings["worktreesDirectory"].as_str().unwrap_or_default().to_owned();
    let add_base = settings["addProjectBaseDirectory"].as_str().unwrap_or_default().to_owned();
    rsx! {
        div { class: "setting-row", id: "storage-worktrees-location", "data-setting": "worktreesDirectory",
            div { class: "font-description", strong { "Worktree location" } p { class: "muted", "Folder where new worktrees are created, such as ~/worktrees. Existing worktrees stay where they are. Leave empty to use the default." } }
            div { class: "font-controls", input { "aria-label": "Worktree location", placeholder: "Default", value: "{location}", disabled: !writable, autocapitalize: "none", spellcheck: "false",
                onchange: { let set = set.clone(); move |event| set("worktreesDirectory", Value::String(event.value().trim().to_owned())) } } }
        }
        div { class: "setting-row", id: "add-project-starts-in", "data-setting": "addProjectBaseDirectory",
            div { class: "font-description", strong { "Add project starts in" } p { class: "muted", "Leave empty to use \"~/\" when the Add Project browser opens." } }
            div { class: "font-controls", input { "aria-label": "Add project starts in", placeholder: "~/", value: "{add_base}", disabled: !writable, autocapitalize: "none", spellcheck: "false",
                onchange: { let set = set.clone(); move |event| set("addProjectBaseDirectory", Value::String(event.value().trim().to_owned())) } } }
        }
        {retention("worktreeAfterDays", "Worktree cleanup", "Delete a worktree after its thread has been inactive this long.")}
        {flag("worktreeOnMerge", "Delete worktrees when merged", "Delete a thread's worktree when its pull request merges.")}
        {flag("worktreeOnDelete", "Delete worktrees with the thread", "Delete a thread's worktree when the thread is deleted.")}
        {flag("worktreeUnchanged", "Delete unchanged worktrees", "Delete worktrees that were never changed.")}
        {retention("browserArtifactsAfterDays", "Artifacts and logs", "Delete browser screenshots and captures after this long.")}
        {retention("logsAfterDays", "Rotated logs", "Delete rotated server logs after this long.")}
    }
}
