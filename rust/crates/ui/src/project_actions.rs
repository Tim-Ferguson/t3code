//! Project actions: the list and editor on the Projects settings page, the Run control in the header of an open thread, and the
//! `script.<id>.run` shortcuts. Port of `ProjectActionsSettings.tsx`, `projectScriptEditor.tsx`, `ProjectScriptsControl.tsx` and
//! `runProjectScript` of `ChatView.tsx`. The decisions are `t3_client::project_scripts`.
//!
//! Differences from the original: an action always runs in a terminal of its own (the original reuses the active terminal when it
//! is idle), an action's preview URL is kept but not offered or opened (there is no preview browser), and importing actions from the
//! checkout's `t3.json` is not offered.
use crate::{
    notice::{self, Level},
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
    settings_common::connected_allows,
    terminal_pane::{Catalog, Launch},
};
use dioxus::prelude::*;
use serde_json::{Value, json};
use t3_client::{keybinding_settings, keybindings, project_scripts as logic, terminal_ui::ScopedPanes};
use t3_contracts::AuthEnvironmentScope;

/// The actions in effect for a thread's project, with the project's id and folder.
pub(crate) fn thread_scripts(state: Store<UiModel>, thread_id: &str) -> Option<(String, String, Vec<Value>)> {
    let field = state.shell();
    let shell = field.read();
    let snapshot = shell.snapshot.as_ref()?;
    let thread = snapshot.threads.iter().find(|thread| thread.id.as_str() == thread_id)?;
    let project = snapshot.projects.iter().find(|project| project.id == thread.project_id)?;
    let legacy: Vec<Value> = project.scripts.iter().filter_map(|script| serde_json::to_value(script).ok()).collect();
    let settings = state.config().read()["settings"].clone();
    Some((project.id.to_string(), project.workspace_root.to_string(), logic::resolve(&settings, Some(project.id.as_str()), &legacy)))
}

/// Runs an action in a new terminal of the thread's drawer, in its worktree or its project's folder (`runProjectScript`). False when
/// the connection may not use terminals or the thread is gone.
pub(crate) fn run(state: Store<UiModel>, catalog: &Catalog, thread_id: &str, script: &Value) -> bool {
    if !connected_allows(state, AuthEnvironmentScope::TerminalOperate) {
        return false;
    }
    let Some(environment) = state.destination().peek().as_ref().map(ToString::to_string) else { return false };
    let Some((_, root, _)) = thread_scripts(state, thread_id) else { return false };
    let worktree = state.shell().peek().snapshot.as_ref().and_then(|snapshot| snapshot.threads.iter().find(|thread| thread.id.as_str() == thread_id)).and_then(|thread| thread.worktree_path.as_ref().map(ToString::to_string));
    let Some(command) = script["command"].as_str().filter(|command| !command.trim().is_empty()) else { return false };
    catalog.run_command(&ScopedPanes::key(&environment, thread_id), Launch { env: logic::runtime_env(&root, worktree.as_deref()), command: command.to_owned() });
    true
}

/// The ids of the actions that have a shortcut command the key runner may run now (the open thread's).
pub(crate) fn runnable_commands(state: Store<UiModel>) -> Vec<String> {
    let Some(thread) = state.active_thread().read().clone() else { return vec![] };
    if !connected_allows(state, AuthEnvironmentScope::TerminalOperate) {
        return vec![];
    }
    thread_scripts(state, &thread).map(|(_, _, scripts)| scripts.iter().filter_map(|script| logic::command_for(script["id"].as_str()?)).collect()).unwrap_or_default()
}

/// `script.<id>.run` from a shortcut or the palette.
pub(crate) fn run_command(state: Store<UiModel>, catalog: &Catalog, command: &str) {
    let (Some(id), Some(thread)) = (logic::id_from_command(command), state.active_thread().peek().clone()) else { return };
    if let Some((_, _, scripts)) = thread_scripts(state, &thread) {
        if let Some(script) = scripts.iter().find(|script| script["id"] == id) {
            run(state, catalog, &thread, script);
        }
    }
}

/// The header's Run button for the open thread's project: the action it runs first (the last one run, else the primary one), and a
/// menu of the others.
#[component]
pub(crate) fn RunControl(state: Store<UiModel>, thread_id: String) -> Element {
    let catalog = try_consume_context::<Catalog>();
    let mut last = use_signal(|| None::<String>);
    let mut menu = use_signal(|| false);
    let Some(catalog) = catalog else { return rsx! {} };
    let Some((_, _, scripts)) = thread_scripts(state, &thread_id) else { return rsx! {} };
    let allowed = connected_allows(state, AuthEnvironmentScope::TerminalOperate);
    let shown = last.read().as_ref().and_then(|id| scripts.iter().find(|script| script["id"] == id.as_str())).or_else(|| logic::primary(&scripts)).cloned();
    let Some(shown) = shown else { return rsx! {} };
    let name = shown["name"].as_str().unwrap_or_default().to_owned();
    let glyph = logic::icon_glyph(shown["icon"].as_str().unwrap_or("play"));
    rsx! {
        div { class: "run-control",
            button { class: "icon-button", "aria-label": "Run {name}", title: "Run {name}", disabled: !allowed,
                onclick: { let (catalog, thread_id, shown) = (catalog.clone(), thread_id.clone(), shown.clone()); move |_| {
                    last.set(shown["id"].as_str().map(str::to_owned));
                    run(state, &catalog, &thread_id, &shown);
                } },
                "{glyph} {name}" }
            if scripts.len() > 1 {
                button { class: "icon-button", "aria-label": "Choose an action", "aria-expanded": "{menu()}", disabled: !allowed, onclick: move |_| menu.set(!menu()), "\u{25be}" }
                if menu() {
                    ul { class: "run-menu", role: "menu", "aria-label": "Actions",
                        for script in scripts.iter() {
                            {
                                let label = logic::menu_label(script);
                                let glyph = logic::icon_glyph(script["icon"].as_str().unwrap_or("play"));
                                let (script, catalog, thread_id) = (script.clone(), catalog.clone(), thread_id.clone());
                                rsx! { li { key: "{script[\"id\"]}", role: "none",
                                    button { role: "menuitem", "aria-label": "Run {label}", onclick: move |_| {
                                        menu.set(false);
                                        last.set(script["id"].as_str().map(str::to_owned));
                                        run(state, &catalog, &thread_id, &script);
                                    }, "{glyph} {label}" } } }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// What the editor holds besides the action itself: its shortcut. `None` is "leave it as it is".
#[derive(Debug, Clone, PartialEq)]
struct Editing {
    /// The action being edited; none for a new one.
    id: Option<String>,
    input: logic::Input,
    shortcut: Option<Option<String>>,
    recording: bool,
}

/// The "Actions" section of the Projects page: the project's list (or the environment's, with no project), with add, edit and delete.
#[component]
pub(crate) fn Section(state: Store<UiModel>, transport: TransportHandle, project: Option<String>) -> Element {
    let mut editing = use_signal(|| None::<Editing>);
    let mut saving = use_signal(|| false);
    let mut failure = use_signal(|| None::<String>);
    let mut confirm_delete = use_signal(|| false);
    let write_ok = connected_allows(state, AuthEnvironmentScope::SettingsWrite);
    let settings = state.config().read()["settings"].clone();
    let legacy: Vec<Value> = project
        .as_deref()
        .and_then(|id| state.shell().read().snapshot.as_ref().and_then(|snapshot| snapshot.projects.iter().find(|candidate| candidate.id.as_str() == id).map(|found| found.scripts.iter().filter_map(|script| serde_json::to_value(script).ok()).collect()))).unwrap_or_default();
    let scripts = logic::resolve(&settings, project.as_deref(), &legacy);
    let inherits = logic::inherits(&settings, project.as_deref(), &legacy);
    let writable = write_ok && !*saving.read() && settings.is_object();
    let rules = {
        let field = state.typed_config();
        let config = field.read();
        config.as_ref().map(|config| keybindings::from_wire(&config.keybindings)).unwrap_or_default()
    };
    let platform = crate::keys::platform();

    // One save: the list, then the shortcut when it changed. Everything is sent in order and the first refusal stops the rest.
    let save = {
        let (transport, project, settings, rules) = (transport.clone(), project.clone(), settings.clone(), rules.clone());
        use_callback(move |(scripts, id, shortcut): (Vec<Value>, Option<String>, Option<Option<String>>)| {
            if *saving.peek() {
                return;
            }
            saving.set(true);
            failure.set(None);
            let (transport, patch) = (transport.clone(), logic::save_patch(&settings, project.as_deref(), scripts));
            let command = id.as_deref().and_then(logic::command_for);
            let previous = command.as_deref().and_then(|command| keybinding_settings::spelling_of(&rules, command));
            spawn(async move {
                let mut result = runtime::request_value_reported(transport.clone(), state, "server.updateSettings", patch).await.map(|_| ());
                if result.is_ok() && !runtime::live_config_active(&transport, state) {
                    result = runtime::request_value_reported(transport.clone(), state, "server.getConfig", json!({})).await.map(|_| ());
                }
                if let (Ok(()), Some(shortcut), Some(command)) = (&result, shortcut, command) {
                    // Shortcuts belong to the environment, and to the action id.
                    let call = match (shortcut, previous.filter(|(key, _)| !key.is_empty())) {
                        (Some(key), previous) => {
                            let mut payload = json!({"command": command, "key": key});
                            if let Some((old, when)) = previous.filter(|(old, _)| *old != key) {
                                payload["replace"] = json!({"command": command, "key": old});
                                if !when.is_empty() {
                                    payload["replace"]["when"] = json!(when);
                                }
                            }
                            Some(("server.upsertKeybinding", payload))
                        }
                        (None, Some((old, when))) => {
                            let mut payload = json!({"command": command, "key": old});
                            if !when.is_empty() {
                                payload["when"] = json!(when);
                            }
                            Some(("server.removeKeybinding", payload))
                        }
                        (None, None) => None,
                    };
                    if let Some((method, payload)) = call {
                        result = runtime::request_value_reported(transport.clone(), state, method, payload).await.map(|_| ());
                    }
                }
                saving.set(false);
                match result {
                    Ok(()) => editing.set(None),
                    Err(message) => {
                        failure.set(Some(format!("Failed to save project actions. {message}")));
                        notice::push(state, Level::Error, "Failed to save project actions", message);
                    }
                }
            });
        })
    };
    let all_ids: Vec<String> = scripts.iter().filter_map(|script| script["id"].as_str().map(str::to_owned)).collect();
    let submit = {
        let (scripts, all_ids) = (scripts.clone(), all_ids.clone());
        move |edit: Editing| {
            if let Some(problem) = edit.input.problem() {
                failure.set(Some(problem.to_owned()));
                return;
            }
            let existing = edit.id.is_some();
            let id = edit.id.clone().unwrap_or_else(|| logic::next_id(&edit.input.name, &all_ids));
            save.call((logic::with_saved(&scripts, &id, &edit.input, existing), Some(id), edit.shortcut.clone()));
        }
    };

    rsx! {
        section { class: "sc-section", id: "project-actions", "aria-label": "Actions", "data-scope": if project.is_some() { "project" } else { "environment" },
            h2 { "Actions" }
            p { class: "muted", "Commands that run in this project's checkout or its worktree, with optional shortcuts." }
            if let Some(message) = failure.read().clone() { p { class: "error-banner", role: "alert", "{message}" } }
            if project.is_some() {
                p { class: "muted",
                    if inherits { "Inherited from the environment. " } else { "This project has its own list. " }
                    if !inherits && write_ok {
                        button { r#type: "button", "aria-label": "Use the environment's actions", disabled: !writable,
                            onclick: { let (transport, project, settings) = (transport.clone(), project.clone(), settings.clone()); move |_| {
                                if let Some(patch) = t3_client::text_settings::clear_patch(&settings, project.as_deref(), logic::SETTING) {
                                    let transport = transport.clone();
                                    saving.set(true);
                                    spawn(async move {
                                        let result = runtime::request_value_reported(transport.clone(), state, "server.updateSettings", patch).await;
                                        if result.is_ok() && !runtime::live_config_active(&transport, state) { let _ = runtime::request_value_reported(transport, state, "server.getConfig", json!({})).await; }
                                        saving.set(false);
                                        if let Err(message) = result { failure.set(Some(format!("Failed to save project actions. {message}"))); }
                                    });
                                }
                            } },
                            "Inherit" }
                    }
                }
            }
            if scripts.is_empty() { p { class: "muted", "No actions configured." } }
            ul { class: "action-list",
                for script in scripts.iter() {
                    {
                        let id = script["id"].as_str().unwrap_or_default().to_owned();
                        let name = script["name"].as_str().unwrap_or_default().to_owned();
                        let command_text = script["command"].as_str().unwrap_or_default().to_owned();
                        let glyph = logic::icon_glyph(script["icon"].as_str().unwrap_or("play"));
                        let label = logic::command_for(&id).and_then(|command| keybindings::shortcut_label(&rules, &command, platform, &keybindings::Context::base(crate::keys::desktop())));
                        let (setup, settle) = (script["runOnWorktreeCreate"] == true, script["runOnSettle"] == true);
                        let edit_script = script.clone();
                        rsx! { li { key: "{id}", class: "action-row",
                            span { class: "action-title", span { "aria-hidden": "true", "{glyph}" } strong { "{name}" }
                                if setup { span { class: "action-badge", "setup" } }
                                if settle { span { class: "action-badge", "on settle" } } }
                            code { class: "action-command", "{command_text}" }
                            if let Some(label) = label { span { class: "muted", "{label}" } }
                            button { r#type: "button", "aria-label": "Edit {name}", disabled: !writable, onclick: move |_| {
                                failure.set(None);
                                confirm_delete.set(false);
                                editing.set(Some(Editing { id: Some(id.clone()), input: logic::Input::of(&edit_script), shortcut: None, recording: false }));
                            }, "Edit" }
                        } }
                    }
                }
            }
            div { class: "field-row",
                button { r#type: "button", "aria-label": "Add action", disabled: !writable, onclick: move |_| { failure.set(None); confirm_delete.set(false); editing.set(Some(Editing { id: None, input: logic::Input::default(), shortcut: None, recording: false })); }, "Add action" }
            }
            if let Some(edit) = editing.read().clone() {
                {
                    let existing = edit.id.clone();
                    let title = if existing.is_some() { "Edit Action" } else { "Add Action" };
                    let command_id = existing.as_deref().or(None).and_then(logic::command_for);
                    let current_key = command_id.as_deref().and_then(|command| keybinding_settings::spelling_of(&rules, command)).map(|(key, _)| key).filter(|key| !key.is_empty());
                    let shown_key = match &edit.shortcut { Some(next) => next.clone(), None => current_key.clone() };
                    let shown_text = shown_key.as_deref().and_then(keybindings::parse_shortcut).map(|shortcut| keybindings::format_shortcut_label(&shortcut, platform)).or(shown_key.clone()).unwrap_or_default();
                    let delete_name = edit.input.name.clone();
                    rsx! {
                        div { class: "dialog action-editor", role: "dialog", "aria-modal": "true", "aria-label": "{title}",
                            h3 { "{title}" }
                            p { class: "muted", "Actions are project-scoped commands you can run from the top bar or keybindings." }
                            fieldset { disabled: *saving.read(),
                                label { r#for: "script-name", "Name" }
                                input { id: "script-name", "aria-label": "Action name", placeholder: "Test", value: "{edit.input.name}", oninput: { let edit = edit.clone(); move |event| editing.set(Some(Editing { input: logic::Input { name: event.value(), ..edit.input.clone() }, ..edit.clone() })) } }
                                label { r#for: "script-icon", "Icon" }
                                select { id: "script-icon", "aria-label": "Action icon", value: "{edit.input.icon}", onchange: { let edit = edit.clone(); move |event| editing.set(Some(Editing { input: logic::Input { icon: event.value(), ..edit.input.clone() }, ..edit.clone() })) },
                                    for (value, label) in logic::ICONS { option { key: "{value}", value: "{value}", selected: edit.input.icon == value, "{label}" } }
                                }
                                label { r#for: "script-command", "Command" }
                                textarea { id: "script-command", "aria-label": "Action command", placeholder: "bun test", value: "{edit.input.command}", oninput: { let edit = edit.clone(); move |event| editing.set(Some(Editing { input: logic::Input { command: event.value(), ..edit.input.clone() }, ..edit.clone() })) } }
                                label { r#for: "script-keybinding", "Keybinding" }
                                input { id: "script-keybinding", "aria-label": "Action keybinding", "data-keybinding-capture": "", readonly: true, placeholder: "Press shortcut", value: if edit.recording { String::new() } else { shown_text.clone() },
                                    onfocus: { let edit = edit.clone(); move |_| editing.set(Some(Editing { recording: true, ..edit.clone() })) },
                                    onblur: { let edit = edit.clone(); move |_| editing.set(Some(Editing { recording: false, ..edit.clone() })) },
                                    onkeydown: { let edit = edit.clone(); move |event| {
                                        if event.key() == Key::Tab { return; }
                                        event.prevent_default();
                                        event.stop_propagation();
                                        if event.key() == Key::Backspace || event.key() == Key::Delete { editing.set(Some(Editing { shortcut: Some(None), ..edit.clone() })); return; }
                                        if let Some(next) = keybindings::binding_from_event(&crate::keybinding_page::key_event(&event), platform) { editing.set(Some(Editing { shortcut: Some(Some(next)), recording: false, ..edit.clone() })); }
                                    } } }
                                p { class: "muted", "Press a shortcut. Use Backspace to clear. Shortcuts are environment-wide. Projects using the same action share its shortcut." }
                                label { class: "action-toggle", span { "Run automatically on worktree creation" }
                                    input { r#type: "checkbox", role: "switch", "aria-label": "Run automatically on worktree creation", checked: edit.input.run_on_worktree_create, onchange: { let edit = edit.clone(); move |event| editing.set(Some(Editing { input: logic::Input { run_on_worktree_create: event.checked(), ..edit.input.clone() }, ..edit.clone() })) } } }
                                label { class: "action-toggle", span { "Wait for it to finish before the agent starts" }
                                    input { r#type: "checkbox", role: "switch", "aria-label": "Wait for it to finish before the agent starts", checked: edit.input.wait_for_setup, disabled: !edit.input.run_on_worktree_create, onchange: { let edit = edit.clone(); move |event| editing.set(Some(Editing { input: logic::Input { wait_for_setup: event.checked(), ..edit.input.clone() }, ..edit.clone() })) } } }
                                label { class: "action-toggle", span { "Run in the thread's worktree when the thread settles" }
                                    input { r#type: "checkbox", role: "switch", "aria-label": "Run in the thread's worktree when the thread settles", checked: edit.input.run_on_settle, onchange: { let edit = edit.clone(); move |event| editing.set(Some(Editing { input: logic::Input { run_on_settle: event.checked(), ..edit.input.clone() }, ..edit.clone() })) } } }
                            }
                            div { class: "dialog-actions",
                                if existing.is_some() { button { r#type: "button", class: "danger", "aria-label": "Delete action", disabled: *saving.read(), onclick: move |_| confirm_delete.set(true), "Delete" } }
                                button { r#type: "button", "aria-label": "Cancel action editor", onclick: move |_| editing.set(None), "Cancel" }
                                button { r#type: "button", class: "primary", "aria-label": "Save action", disabled: *saving.read(), onclick: { let edit = edit.clone(); let mut submit = submit.clone(); move |_| submit(edit.clone()) },
                                    if *saving.read() { "Saving\u{2026}" } else if existing.is_some() { "Save changes" } else { "Save action" } }
                            }
                            if confirm_delete() {
                                crate::settings_common::Confirm { label: "Delete action".to_owned(), title: format!("Delete action \"{delete_name}\"?"), body: "This action cannot be undone.".to_owned(), confirm: "Delete".to_owned(), busy: *saving.read(),
                                    oncancel: move |_| confirm_delete.set(false),
                                    onconfirm: { let (scripts, edit) = (scripts.clone(), edit.clone()); move |_| {
                                        confirm_delete.set(false);
                                        let Some(id) = edit.id.clone() else { return };
                                        let rest: Vec<Value> = scripts.iter().filter(|script| script["id"] != id.as_str()).cloned().collect();
                                        save.call((rest, Some(id), Some(None)));
                                    } } }
                            }
                        }
                    }
                }
            }
        }
    }
}
