//! Settings, Keybindings: the shortcuts of the connected environment, each with its condition,
//! where it came from and what it collides with; record a new one, reset or remove a custom one,
//! add one, and see what is wrong with the file. Port of `KeybindingsSettings.tsx`, limited to the
//! commands this app has. Everything is read from, and written to, the environment that delivered
//! the configuration, and only with its `settings:write` grant.
use crate::{
    keys::Keys,
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt, View},
};
use dioxus::prelude::*;
use serde_json::{Value, json};
use t3_client::{
    connection::ConnectionStatus,
    keybinding_settings::{self as settings, Row, Source, Written},
    keybindings::{self, KeyEvent},
};
use t3_contracts::{AuthEnvironmentScope, ServerConfigIssue};

pub(crate) const BROWSER_NOTICE: &str = "The browser may claim some shortcuts first. The desktop app receives them all.";
pub(crate) const READ_ONLY: &str = "This connection can view keybindings but cannot change them.";

/// A key press as the keybinding model sees it.
pub(crate) fn key_event(event: &KeyboardEvent) -> KeyEvent {
    let modifiers = event.modifiers();
    KeyEvent {
        key: event.key().to_string(),
        code: event.code().to_string(),
        meta: modifiers.contains(Modifiers::META),
        ctrl: modifiers.contains(Modifiers::CONTROL),
        shift: modifiers.contains(Modifiers::SHIFT),
        alt: modifiers.contains(Modifiers::ALT),
        repeat: event.is_auto_repeating(),
        composing: event.is_composing(),
        ..Default::default()
    }
}

fn can_write(state: Store<UiModel>) -> bool {
    *state.status().read() == ConnectionStatus::Connected && state.destination().read().as_ref().is_some_and(|destination| state.environments().read().allows(destination, AuthEnvironmentScope::SettingsWrite))
}

/// One thing the page is editing: a row's key and condition, or a new rule.
#[derive(Debug, Clone, PartialEq)]
struct Draft {
    command: String,
    key: String,
    when: String,
    recording: bool,
}

#[component]
pub(crate) fn Page(state: Store<UiModel>, transport: TransportHandle) -> Element {
    let keys = use_context::<Keys>();
    let destination = state.destination().read().clone();
    let connected = *state.status().read() == ConnectionStatus::Connected && destination.is_some();
    let environment_label = destination.as_ref().and_then(|id| state.environments().read().records.get(id).map(|record| record.label.clone())).unwrap_or_default();
    rsx! { section { class: "settings-page keybindings-page",
        crate::settings_common::Tabs { state, current: View::Keybindings }
        h1 { "Keybindings" }
        if connected {
            Editor { key: "{destination.as_ref().map(ToString::to_string).unwrap_or_default()}", state, transport, keys, environment_label }
        } else {
            p { "Connect to an environment to see and change its keybindings." }
            button { onclick: move |_| state.view().set(View::Connections), "Open connections" }
        }
    } }
}

#[component]
fn Editor(state: Store<UiModel>, transport: TransportHandle, keys: Keys, environment_label: String) -> Element {
    let mut query = use_signal(String::new);
    let mut draft = use_signal(|| None::<(String, Draft)>);
    let mut adding = use_signal(|| None::<Draft>);
    let mut saving = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);
    let mut written = use_signal(Written::default);
    let new_rule_key = "new-keybinding";
    let writable = can_write(state);
    let rules = keys.rules.read().clone();
    let rows = settings::build_rows(&rules, &query.read());
    let hidden = settings::hidden_count(&rules);
    let (path, issues) = {
        let field = state.typed_config();
        let config = field.read();
        match config.as_ref() {
            Some(config) => (config.keybindings_config_path.to_string(), config.issues.0.clone()),
            None => (String::new(), vec![]),
        }
    };

    // Saving and removing share one path: send, then apply what the server returns. A reply for a
    // connection that has since changed is dropped.
    let send = {
        let transport = transport.clone();
        use_callback(move |(method, payload, remember): (&'static str, Value, Option<(String, String, String)>)| {
            if *saving.peek() || !can_write(state) {
                return;
            }
            let Some(owner) = runtime::connection_owner(&transport, state) else { return };
            saving.set(true);
            error.set(None);
            let transport = transport.clone();
            spawn(async move {
                let result = runtime::request_value_reported(transport.clone(), state, method, payload).await;
                // The flag is this page's own, so it is released even when the reply is not wanted.
                saving.set(false);
                if runtime::connection_owner(&transport, state).as_ref() != Some(&owner) {
                    return;
                }
                match result {
                    Ok(value) => {
                        if let Some((command, key, when)) = remember {
                            written.write().remember(&command, &key, &when);
                        }
                        apply(state, &value);
                        draft.set(None);
                        adding.set(None);
                    }
                    Err(message) => error.set(Some(format!("{} {message}", if method == "server.removeKeybinding" { "Unable to remove keybinding." } else { "Unable to save keybinding." }))),
                }
            });
        })
    };
    let upsert = move |command: String, key: String, when: String, replace: Option<(String, String)>| {
        let mut payload = json!({"command":command,"key":key.trim()});
        if !when.trim().is_empty() {
            payload["when"] = json!(when.trim());
        }
        if let Some((replace_key, replace_when)) = replace {
            payload["replace"] = json!({"command":command,"key":replace_key});
            if !replace_when.trim().is_empty() {
                payload["replace"]["when"] = json!(replace_when);
            }
        }
        send.call(("server.upsertKeybinding", payload, Some((command, key.trim().to_owned(), when.trim().to_owned()))));
    };
    let mut remove = move |row: &Row| {
        let (key, when) = written.peek().text_for(row);
        let mut payload = json!({"command":row.command,"key":key});
        if !when.trim().is_empty() {
            payload["when"] = json!(when);
        }
        written.write().forget(&row.command, &row.key, &row.when);
        send.call(("server.removeKeybinding", payload, None));
    };

    rsx! {
        p { class: "muted", "Shortcuts of {environment_label}. Changes are saved to its keybindings file." }
        if cfg!(target_arch = "wasm32") { p { class: "muted keybinding-browser-note", title: BROWSER_NOTICE, "\u{26a0} {BROWSER_NOTICE}" } }
        if !path.is_empty() && !issues.is_empty() {
            div { class: "keybinding-issues", role: "alert",
                strong { "The keybindings file has problems" }
                p { class: "muted", "{path}" }
                ul { for (index, issue) in issues.iter().enumerate() { li { key: "{index}",
                    match issue { ServerConfigIssue::MalformedKeybindings { message } => format!("{message}"), ServerConfigIssue::InvalidKeybinding { message, index } => format!("Entry {} is ignored: {message}", index.as_u64().map_or_else(|| index.to_string(), |n| (n + 1).to_string())) } } } }
            }
        }
        if !writable { p { class: "muted", role: "status", "{READ_ONLY}" } }
        if let Some(message) = error.read().clone() { p { class: "error-banner", role: "alert", "{message}" } }
        if *saving.read() { p { role: "status", "Saving keybindings\u{2026}" } }
        div { class: "keybinding-toolbar",
            input { r#type: "search", "aria-label": "Search keybindings", placeholder: "Search keybindings", value: "{query}", oninput: move |event| query.set(event.value()),
                onkeydown: move |event| { if event.key() == Key::Escape && !query.peek().is_empty() { event.stop_propagation(); event.prevent_default(); query.set(String::new()); } } }
            button { "aria-label": "Add keybinding", disabled: adding.read().is_some() || !writable, onclick: move |_| adding.set(Some(Draft { command: settings::command_options().first().map(|(command, _)| (*command).to_owned()).unwrap_or_default(), key: String::new(), when: String::new(), recording: false })), "Add keybinding" }
        }
        if let Some(new) = adding.read().clone() {
            NewRule { key: "{new_rule_key}", draft: new, rows: rows.clone(), saving: *saving.read(), onchange: move |value| adding.set(Some(value)), oncancel: move |_| adding.set(None), onsave: move |value: Draft| upsert(value.command, value.key, value.when, None) }
        }
        fieldset { disabled: !writable, class: "keybinding-groups",
            if rows.is_empty() { p { class: "muted", role: "status", "No keybindings match your search." } }
            for group in settings::group_rows(rows.clone()) {
                section { key: "{group.id}", class: "keybinding-group",
                    h2 { "{group.title}" }
                    for row in group.rows {
                        RowView { key: "{row.id}", row: row.clone(), all: rows.clone(), editing: draft.read().as_ref().filter(|(id, _)| *id == row.id).map(|(_, value)| value.clone()), saving: *saving.read(), writable,
                            onedit: { let row = row.clone(); move |value: Option<Draft>| draft.set(value.map(|value| (row.id.clone(), value))) },
                            onsave: { let row = row.clone(); move |value: Draft| { let replace = written.peek().text_for(&row); upsert(value.command, value.key, value.when, Some(replace)) } },
                            onreset: { let row = row.clone(); move |_| { if let Some(default_key) = row.default_key.clone() { let replace = written.peek().text_for(&row); upsert(row.command.clone(), default_key, row.default_when.clone(), Some(replace)) } } },
                            onremove: { let row = row.clone(); move |_| remove(&row) } }
                    }
                }
            }
        }
        if hidden > 0 { p { class: "muted", "{hidden} more shortcuts in the file are for features this app does not have yet. They are kept and do nothing here." } }
    }
}

/// Puts the server's answer to a save into the configuration the rest of the app reads.
fn apply(state: Store<UiModel>, result: &Value) {
    let Ok(rules) = serde_json::from_value::<t3_contracts::ResolvedKeybindingsConfig>(result["keybindings"].clone()) else { return };
    let mut config = state.typed_config();
    let mut config = config.write();
    if let Some(config) = config.as_mut() {
        config.keybindings = rules;
    }
}

#[component]
fn RowView(row: Row, all: Vec<Row>, editing: Option<Draft>, saving: bool, writable: bool, onedit: EventHandler<Option<Draft>>, onsave: EventHandler<Draft>, onreset: EventHandler<()>, onremove: EventHandler<()>) -> Element {
    let label = settings::command_label(&row.command);
    let current = editing.clone().unwrap_or(Draft { command: row.command.clone(), key: row.key.clone(), when: row.when.clone(), recording: false });
    let dirty = current.key != row.key || current.when != row.when;
    let when_valid = current.when.trim().is_empty() || keybindings::parse_when(&current.when).is_some();
    let conflicts = if dirty { settings::conflict_labels(&all, &row.id, &current.key, &current.when) } else { row.conflicts.clone() };
    let pill = keybindings::parse_shortcut(&row.key).map(|shortcut| keybindings::format_shortcut_label(&shortcut, crate::keys::platform())).unwrap_or_else(|| row.key.clone());
    let unknown = settings::unknown_when_variables(&current.when);
    let platform = crate::keys::platform();
    rsx! {
        div { class: "keybinding-row", "data-command": "{row.command}",
            div { class: "keybinding-title",
                strong { "{label}" }
                if row.source == Source::Custom { span { class: "keybinding-source", "Custom" } }
            }
            div { class: "keybinding-controls",
                if dirty { button { class: "primary", "aria-label": "Save keybinding for {label}", disabled: saving || current.key.trim().is_empty() || !when_valid, onclick: { let current = current.clone(); move |_| onsave.call(current.clone()) }, if saving { "Saving" } else { "Save" } } }
                if !current.recording && !dirty {
                    button { class: "keybinding-pill", "aria-label": "Edit shortcut for {label}: {pill}", onclick: { let current = current.clone(); move |_| onedit.call(Some(Draft { recording: true, ..current.clone() })) }, "{row.key}" }
                } else {
                    input { "data-keybinding-capture": "", "aria-label": "Keybinding for {label}", placeholder: if current.recording { "Press shortcut" } else { "Unassigned" }, value: if current.recording { String::new() } else { current.key.clone() },
                        onmounted: move |event| { spawn(async move { let _ = event.set_focus(true).await; }); },
                        onfocus: { let current = current.clone(); move |_| { if !current.recording { onedit.call(Some(Draft { recording: true, ..current.clone() })) } } },
                        onblur: { let current = current.clone(); let row = row.clone(); move |_| { if current.recording { onedit.call(if current.key == row.key && current.when == row.when { None } else { Some(Draft { recording: false, ..current.clone() }) }) } } },
                        oninput: { let current = current.clone(); move |event| onedit.call(Some(Draft { key: event.value(), ..current.clone() })) },
                        onkeydown: { let current = current.clone(); let row = row.clone(); move |event| {
                            // Tab moves on unless a shortcut is being recorded; then it is recorded like any key.
                            if event.key() == Key::Tab && !current.recording { return; }
                            event.prevent_default();
                            event.stop_propagation();
                            if event.key() == Key::Escape { onedit.call(if current.when == row.when { None } else { Some(Draft { key: row.key.clone(), recording: false, ..current.clone() }) }); return; }
                            if let Some(next) = keybindings::binding_from_event(&key_event(&event), platform) {
                                onedit.call(Some(Draft { key: next, recording: false, ..current.clone() }));
                            }
                        } } }
                }
                input { class: "keybinding-when", "aria-label": "When expression for {label}", placeholder: "Always", value: "{current.when}", disabled: !writable,
                    oninput: { let current = current.clone(); move |event| onedit.call(Some(Draft { when: event.value(), ..current.clone() })) } }
                if row.source == Source::Custom && row.default_key.is_some() { button { "aria-label": "Reset {label} to default", disabled: saving, onclick: move |_| onreset.call(()), "Reset" } }
                if row.source == Source::Custom { button { "aria-label": "Remove shortcut for {label}", disabled: saving, onclick: move |_| onremove.call(()), "Remove" } }
            }
            if !when_valid { p { class: "keybinding-problem", role: "alert", "Use variables with !, &&, ||, and parentheses." } }
            if !unknown.is_empty() { p { class: "keybinding-problem", role: "status", "Unknown condition: {unknown.join(\", \")}" } }
            if !conflicts.is_empty() { p { class: "keybinding-conflict", role: "status", "Conflicts with {conflicts.join(\", \")}" } }
        }
    }
}

#[component]
fn NewRule(draft: Draft, rows: Vec<Row>, saving: bool, onchange: EventHandler<Draft>, oncancel: EventHandler<()>, onsave: EventHandler<Draft>) -> Element {
    let when_valid = draft.when.trim().is_empty() || keybindings::parse_when(&draft.when).is_some();
    let conflicts = settings::conflict_labels(&rows, "", &draft.key, &draft.when);
    let platform = crate::keys::platform();
    let options = settings::command_options();
    rsx! {
        div { class: "keybinding-row new", role: "group", "aria-label": "New keybinding",
            div { class: "keybinding-controls",
                select { "aria-label": "Command", value: "{draft.command}", onchange: { let draft = draft.clone(); move |event| onchange.call(Draft { command: event.value(), ..draft.clone() }) },
                    for (command, label) in options { option { key: "{command}", value: "{command}", selected: command == draft.command, "{label}" } }
                }
                input { "data-keybinding-capture": "", "aria-label": "Keybinding for {settings::command_label(&draft.command)}", placeholder: if draft.recording { "Press shortcut" } else { "Unassigned" }, value: if draft.recording { String::new() } else { draft.key.clone() },
                    onfocus: { let draft = draft.clone(); move |_| onchange.call(Draft { recording: true, ..draft.clone() }) },
                    onblur: { let draft = draft.clone(); move |_| onchange.call(Draft { recording: false, ..draft.clone() }) },
                    oninput: { let draft = draft.clone(); move |event| onchange.call(Draft { key: event.value(), ..draft.clone() }) },
                    onkeydown: { let draft = draft.clone(); move |event| {
                        if event.key() == Key::Tab && !draft.recording { return; }
                        event.prevent_default();
                        event.stop_propagation();
                        if event.key() == Key::Escape { oncancel.call(()); return; }
                        if let Some(next) = keybindings::binding_from_event(&key_event(&event), platform) { onchange.call(Draft { key: next, recording: false, ..draft.clone() }); }
                    } } }
                input { class: "keybinding-when", "aria-label": "When expression for the new keybinding", placeholder: "Always", value: "{draft.when}", oninput: { let draft = draft.clone(); move |event| onchange.call(Draft { when: event.value(), ..draft.clone() }) } }
                button { class: "primary", "aria-label": "Save new keybinding", disabled: saving || draft.key.trim().is_empty() || !when_valid, onclick: { let draft = draft.clone(); move |_| onsave.call(draft.clone()) }, if saving { "Saving" } else { "Save" } }
                button { "aria-label": "Cancel new keybinding", onclick: move |_| oncancel.call(()), "Cancel" }
            }
            if !when_valid { p { class: "keybinding-problem", role: "alert", "Use variables with !, &&, ||, and parentheses." } }
            if !conflicts.is_empty() { p { class: "keybinding-conflict", role: "status", "Conflicts with {conflicts.join(\", \")}" } }
        }
    }
}
