//! The icon of the connected environment (`EnvironmentIconPicker.tsx`): which kind of machine it is drawn as. The server detects a
//! kind and says so in its descriptor; a choice other than the detected kind is kept in the environment's settings
//! (`environmentIcon`), and choosing the detected kind clears it. The control says why it is locked when it is, in the original's
//! words, instead of disappearing.
use crate::{
    notice::{self, Level},
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
    settings_common::connected_allows,
};
use dioxus::prelude::*;
use serde_json::{Value, json};
use t3_contracts::AuthEnvironmentScope;

/// The kinds a machine can be drawn as, with the labels the original gives them.
pub(crate) const KINDS: [(&str, &str); 7] = [("server", "Server"), ("cloud", "Cloud VM"), ("linux", "Linux/WSL"), ("desktop", "Desktop"), ("laptop", "Laptop"), ("mac-mini", "Mini PC"), ("mac-studio", "Workstation")];

/// `resolveEnvironmentIconPickerLock`: why the picker is inert, in the order a person can do something about it.
pub(crate) fn lock(connected: bool, supported: bool, may_write: bool) -> Option<&'static str> {
    if !connected {
        Some("Connect to this environment to change its icon.")
    } else if !supported {
        Some("This environment's server is too old to keep an icon. Update it to choose one.")
    } else if !may_write {
        Some("Your session on this environment cannot change its settings.")
    } else {
        None
    }
}

/// The kind in force: the person's choice, else the server's detection, else a server.
pub(crate) fn resolved(config: &Value) -> String {
    let chosen = config["settings"]["environmentIcon"].as_str();
    let detected = config["environment"]["platform"]["machine"].as_str();
    chosen.or(detected).filter(|kind| KINDS.iter().any(|(known, _)| known == kind)).unwrap_or("server").to_owned()
}

#[component]
pub(crate) fn Picker(state: Store<UiModel>, transport: TransportHandle) -> Element {
    let config = state.config().read().clone();
    let connected = config["environment"].is_object();
    let supported = config["environment"]["capabilities"]["environmentIcon"] == true;
    let locked = lock(connected, supported, connected_allows(state, AuthEnvironmentScope::SettingsWrite));
    if !connected {
        return rsx! {};
    }
    let detected = config["environment"]["platform"]["machine"].as_str().unwrap_or("server").to_owned();
    let current = resolved(&config);
    let detection = if config["environment"]["platform"]["machine"].is_string() { " (detected)" } else { " (default)" };
    let pick = {
        let detected = detected.clone();
        move |kind: String| {
            if locked.is_some() || !KINDS.iter().any(|(known, _)| *known == kind) {
                return;
            }
            // The detected kind is the default: choosing it clears the choice.
            let value = if kind == detected { Value::Null } else { json!(kind) };
            let transport = transport.clone();
            spawn(async move {
                if let Err(message) = runtime::request_value_reported(transport, state, "server.updateSettings", json!({"patch": {"environmentIcon": value}})).await {
                    notice::push(state, Level::Error, "Could not change the environment icon", message);
                }
            });
        }
    };
    rsx! { div { class: "setting-row", id: "environment-icon", "data-setting": "environmentIcon",
        div { class: "font-description", strong { "Environment icon" } p { class: "muted", "How this environment is drawn in lists of machines." }
            if let Some(reason) = locked { p { class: "muted", role: "status", "{reason}" } } }
        div { class: "font-controls",
            select { "aria-label": "Environment icon", value: "{current}", disabled: locked.is_some(), onchange: move |event| pick(event.value()),
                for (kind, label) in KINDS {
                    option { key: "{kind}", value: kind, selected: kind == current, "{label}", if kind == detected { "{detection}" } }
                }
            }
        }
    } }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_picker_is_locked_for_the_reason_a_person_can_most_easily_fix() {
        assert_eq!(lock(false, false, false), Some("Connect to this environment to change its icon."));
        assert_eq!(lock(true, false, false), Some("This environment's server is too old to keep an icon. Update it to choose one."));
        assert_eq!(lock(true, true, false), Some("Your session on this environment cannot change its settings."));
        assert_eq!(lock(true, true, true), None);
    }

    #[test]
    fn a_choice_outranks_the_detection_and_an_unknown_kind_is_a_server() {
        assert_eq!(resolved(&json!({"settings": {"environmentIcon": "laptop"}, "environment": {"platform": {"machine": "desktop"}}})), "laptop");
        assert_eq!(resolved(&json!({"settings": {"environmentIcon": null}, "environment": {"platform": {"machine": "desktop"}}})), "desktop");
        assert_eq!(resolved(&json!({"settings": {}, "environment": {"platform": {}}})), "server");
        assert_eq!(resolved(&json!({"settings": {"environmentIcon": "toaster"}, "environment": {"platform": {}}})), "server");
    }
}
