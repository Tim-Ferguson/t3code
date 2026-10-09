//! The text generation settings page: the model that writes thread titles and other text, the
//! model and style that write source control text, and how new worktree branches are named, for
//! the environment or for one project that overrides them. Port of the "Text generation"
//! sections of `SettingsPanels.tsx`, `SourceControlWritingSettings.tsx`, `BranchNamingSettings.tsx`
//! and the scoped-settings planning of `scopedSettings.ts`.
//!
//! Only `codex` and `claudeAgent` instances that do not say they cannot write text are offered
//! (an ACP Registry instance never is). Every change is one atomic `server.updateSettings` patch,
//! needs the settings grant, and is sent once: the controls wait for the server's answer.
use crate::{
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt, View},
    settings_common::{Tabs, connected_allows},
};
use dioxus::prelude::*;
use serde_json::{Value, json};
use t3_client::text_settings as logic;
use t3_contracts::AuthEnvironmentScope;

pub(crate) const READ_ONLY: &str = "This connection can view these settings but cannot change them.";

#[component]
pub(crate) fn Page(state: Store<UiModel>, transport: TransportHandle) -> Element {
    let mut scope = use_signal(String::new);
    let mut saving = use_signal(|| false);
    let mut failure = use_signal(|| None::<String>);
    let settings_write = use_memo(move || connected_allows(state, AuthEnvironmentScope::SettingsWrite));
    let readable = use_memo(move || connected_allows(state, AuthEnvironmentScope::OrchestrationRead));
    let config = state.config().read().clone();
    let settings = config["settings"].clone();
    let providers = config["providers"].clone();
    let overrides_supported = config["environment"]["capabilities"]["projectSettingsOverrides"] == true;
    let projects: Vec<(String, String)> = state.shell().read().snapshot.as_ref().map(|snapshot| snapshot.projects.iter().map(|project| (project.id.to_string(), project.title.to_string())).collect()).unwrap_or_default();
    let project = Some(scope.read().clone()).filter(|id| !id.is_empty() && overrides_supported && projects.iter().any(|(project, _)| project == id));
    let choices = logic::choices(&providers);
    let writable = settings_write() && !*saving.read() && config.is_object() && config["settings"].is_object();

    // One patch at a time; the page reads the new value from the configuration the server sends
    // (its own stream, else one request), never from what was typed.
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
                    failure.set(Some(format!("Could not save text generation settings. {message}")));
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
    // A row's note about where its value comes from, when a project is chosen.
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

    let current_model = logic::effective(&settings, project.as_deref(), logic::TEXT_GENERATION_MODEL);
    let current_key = logic::key_of(&current_model);
    let writer_own = logic::effective(&settings, project.as_deref(), logic::WRITER_MODEL);
    let dedicated = writer_own.is_object();
    let writer_key = logic::key_of(&logic::writer_model(&settings, project.as_deref()));
    let style = logic::effective(&settings, project.as_deref(), logic::WRITING_STYLE);
    let mode = style["mode"].as_str().unwrap_or("repo_conventions").to_owned();
    let instructions = style["customInstructions"].as_str().unwrap_or_default().to_owned();
    let templates = style["followChangeRequestTemplates"] != false;
    let branch_mode = logic::effective(&settings, project.as_deref(), logic::BRANCH_MODE).as_str().unwrap_or("static").to_owned();
    let prefix = logic::effective(&settings, project.as_deref(), logic::BRANCH_PREFIX).as_str().unwrap_or_default().to_owned();
    let branch_instructions = logic::effective(&settings, project.as_deref(), logic::BRANCH_INSTRUCTIONS).as_str().unwrap_or_default().to_owned();
    let mode_note = logic::WRITING_MODES.iter().find(|(id, ..)| *id == mode).map(|(.., note)| *note).unwrap_or_default();
    let model_options = |selected: &str| -> Element {
        let known = choices.iter().any(|choice| choice.key() == selected);
        rsx! {
            if !known && selected != "[null,null]" { option { value: "{selected}", selected: true, "Current (not offered)" } }
            for choice in choices.iter() { option { key: "{choice.key()}", value: "{choice.key()}", selected: choice.key() == selected, "{choice.label()}" } }
        }
    };
    let pick = {
        let choices = choices.clone();
        move |key: String| choices.iter().find(|choice| choice.key() == key).map(|choice| choice.selection())
    };

    rsx! {
        section { class: "settings-page", Tabs { state, current: View::TextGeneration }
            h1 { "Text generation" }
            p { class: "muted", "Models and writing style for generated text, and how new worktree branches are named." }
            if !settings_write() { p { class: "muted", role: "status", "{READ_ONLY}" } }
            if let Some(message) = failure.read().clone() { p { class: "error-banner", role: "alert", "{message}" } }
            if *saving.read() { p { role: "status", "Saving text generation settings\u{2026}" } }
            if !readable() || !config.is_object() || !config["settings"].is_object() {
                p { class: "muted", role: "status", "Connect an environment to choose its text generation settings." }
            } else {
                div { class: "field-row",
                    label { r#for: "text-scope", "Settings for" }
                    select { id: "text-scope", "aria-label": "Text generation scope", value: "{scope}", disabled: *saving.read(), onchange: move |event| scope.set(event.value()),
                        option { value: "", selected: scope.read().is_empty(), "Environment defaults" }
                        if overrides_supported { for (id, title) in projects.iter() { option { key: "{id}", value: "{id}", selected: *scope.read() == *id, "Project: {title}" } } }
                    }
                }
                section { class: "sc-section", "aria-label": "Text generation model",
                    h2 { "Text generation" }
                    if choices.is_empty() { p { class: "muted", role: "status", "No text generation providers available." } }
                    else {
                        label { r#for: "text-model", "Model" }
                        select { id: "text-model", "aria-label": "Text generation model", value: "{current_key}", disabled: !writable,
                            onchange: { let (pick, set) = (pick.clone(), set.clone()); move |event| if let Some(selection) = pick(event.value()) { set(logic::TEXT_GENERATION_MODEL, selection) } },
                            {model_options(&current_key)} }
                        p { class: "muted", "Used for thread titles and other generated text on connected devices with this provider. Source control can override it. " {source(logic::TEXT_GENERATION_MODEL)} }
                    }
                }
                section { class: "sc-section", "aria-label": "Source control writing",
                    h2 { "Source control writing" }
                    label { r#for: "writing-style", "Writing style" }
                    select { id: "writing-style", "aria-label": "Source control writing style", value: "{mode}", disabled: !writable,
                        onchange: { let (set, instructions) = (set.clone(), instructions.clone()); move |event| set(logic::WRITING_STYLE, json!({"mode":event.value(),"customInstructions":instructions})) },
                        for (id, label, _) in logic::WRITING_MODES { option { key: "{id}", value: "{id}", selected: id == mode, "{label}" } } }
                    p { class: "muted", "{mode_note} " {source(logic::WRITING_STYLE)} }
                    if mode == "custom" {
                        label { r#for: "writing-instructions", "Instructions" }
                        textarea { id: "writing-instructions", "aria-label": "Custom source control writing instructions", rows: "4", placeholder: "Keep titles concise. Use short bullet points in descriptions.", value: "{instructions}", disabled: !writable,
                            onchange: { let (set, instructions) = (set.clone(), instructions.clone()); move |event| { let text = event.value().trim().to_owned(); if text != instructions { set(logic::WRITING_STYLE, json!({"customInstructions":text})) } } } }
                    }
                    label { class: "switch-row",
                        input { r#type: "checkbox", role: "switch", "aria-label": "Follow change request templates", checked: templates, disabled: !writable,
                            onchange: { let set = set.clone(); move |event| set(logic::WRITING_STYLE, json!({"followChangeRequestTemplates":event.checked()})) } }
                        " Follow change request templates" }
                    p { class: "muted", "{logic::TEMPLATES_NOTE}" }
                    label { class: "switch-row",
                        input { r#type: "checkbox", role: "switch", "aria-label": "Use a separate source control writer model", checked: dedicated, disabled: !writable || (!dedicated && choices.is_empty()),
                            onchange: { let set = set.clone(); let fallback = logic::effective(&settings, project.as_deref(), logic::TEXT_GENERATION_MODEL); move |event| set(logic::WRITER_MODEL, if event.checked() { json!({"instanceId":fallback["instanceId"],"model":fallback["model"]}) } else { Value::Null }) } }
                        " Use a separate source control writer model" }
                    p { class: "muted", "{logic::WRITER_NOTE} " {source(logic::WRITER_MODEL)} }
                    if dedicated {
                        label { r#for: "writer-model", "Writer model" }
                        select { id: "writer-model", "aria-label": "Source control writer model", value: "{writer_key}", disabled: !writable,
                            onchange: { let (pick, set) = (pick.clone(), set.clone()); move |event| if let Some(selection) = pick(event.value()) { set(logic::WRITER_MODEL, selection) } },
                            {model_options(&writer_key)} }
                    }
                }
                section { class: "sc-section", "aria-label": "Worktree branch naming",
                    h2 { "Worktree branch naming" }
                    label { r#for: "branch-mode", "Choose how new worktree branches are named from your first message." }
                    select { id: "branch-mode", "aria-label": "Worktree branch naming", value: "{branch_mode}", disabled: !writable,
                        onchange: { let set = set.clone(); move |event| set(logic::BRANCH_MODE, json!(event.value())) },
                        for (id, label) in logic::BRANCH_MODES { option { key: "{id}", value: "{id}", selected: id == branch_mode, "{label}" } } }
                    p { class: "muted", {source(logic::BRANCH_MODE)} }
                    if branch_mode == "static" {
                        label { r#for: "branch-prefix", "Branch prefix" }
                        input { id: "branch-prefix", "aria-label": "Branch prefix", autocapitalize: "none", spellcheck: "false", placeholder: "No prefix", value: "{prefix}", disabled: !writable,
                            onchange: { let (set, prefix) = (set.clone(), prefix.clone()); move |event| { let text = event.value().trim().to_owned(); if text != prefix { set(logic::BRANCH_PREFIX, json!(text)) } } } }
                        p { class: "muted", "{logic::PREFIX_NOTE} " {source(logic::BRANCH_PREFIX)} }
                    }
                    if branch_mode == "semantic" { p { class: "muted", "{logic::SEMANTIC_NOTE}" } }
                    if branch_mode == "custom" {
                        label { r#for: "branch-instructions", "Branch naming instructions" }
                        textarea { id: "branch-instructions", "aria-label": "Branch naming instructions", rows: "4", placeholder: "Use julius/ followed by the issue ID and a short description.", value: "{branch_instructions}", disabled: !writable,
                            onchange: { let (set, current) = (set.clone(), branch_instructions.clone()); move |event| { let text = event.value().trim().to_owned(); if text != current { set(logic::BRANCH_INSTRUCTIONS, json!(text)) } } } }
                        p { class: "muted", "{logic::INSTRUCTIONS_NOTE} " {source(logic::BRANCH_INSTRUCTIONS)} }
                    }
                }
            }
        }
    }
}
