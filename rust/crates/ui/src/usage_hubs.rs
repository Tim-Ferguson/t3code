//! The hubs whose accounts the Limits views show, and the provider health check interval, on the
//! Providers page. Port of `UsageProviderSettings.tsx`, `AddUsageLimitSourceDialog.tsx` and the
//! Advanced section of `ProviderSettingsPanel.tsx`. The key is sent once; the server keeps it in
//! its secret store and settings carry only a redaction marker afterwards, so nothing here ever
//! reads it back. Cursor account usage (the macOS Keychain switch of the same section) belongs to
//! Cursor and is not built.
use crate::settings_common::Confirm;
use crate::runtime::{UiModel, UiModelStoreExt};
use dioxus::prelude::*;
use serde_json::Value;
use t3_client::{text_settings as interval, usage_limits as logic};

/// The hub list with its Add hub dialog and the confirmation Remove asks for. A write goes out
/// through `onupdate`, the page's single settings writer.
#[component]
pub(crate) fn UsageProviders(settings: Value, environment: String, allowed: bool, onupdate: EventHandler<Value>) -> Element {
    let mut adding = use_signal(|| false);
    let mut removing = use_signal(|| None::<(String, String)>);
    let hubs = logic::hubs_from(&settings["usageLimitSources"]);
    rsx! {
        section { id: "usage-providers", class: "sc-section", "aria-label": "Usage providers",
            header {
                h2 { "Usage providers" }
                if allowed { button { r#type: "button", "aria-label": "Add hub", onclick: move |_| adding.set(true), "\u{ff0b} Add hub" } }
            }
            if hubs.is_empty() { p { class: "muted", "No hubs configured." } }
            for hub in hubs {
                div { key: "{hub.id}", class: "provider-field",
                    div { strong { "{hub.title}" } p { class: "muted usage-hub-address", "{hub.description}" } }
                    if allowed { button { r#type: "button", "aria-label": "Remove {hub.title}", onclick: { let (id, title) = (hub.id.clone(), hub.title.clone()); move |_| removing.set(Some((id.clone(), title.clone()))) }, "Remove" } }
                }
            }
        }
        if *adding.read() && allowed {
            AddHub { environment, onclose: move |_| adding.set(false), onsave: move |patch| { adding.set(false); onupdate.call(patch); } }
        }
        if let Some((id, title)) = removing.read().clone() {
            Confirm { label: "hub removal", title: format!("Remove {title}?"),
                body: "The hub's management key is deleted from this server. Its accounts leave the Limits view; the hub itself is untouched. Add it again with the URL and key to bring them back.",
                confirm: "Remove hub", busy: !allowed,
                oncancel: move |_| removing.set(None),
                onconfirm: move |_| { removing.set(None); onupdate.call(logic::remove_hub_patch(&id)); } }
        }
    }
}

/// Adds a CLIProxyAPI hub to one environment.
#[component]
fn AddHub(environment: String, onclose: EventHandler<()>, onsave: EventHandler<Value>) -> Element {
    let mut url = use_signal(String::new);
    let mut key = use_signal(String::new);
    let mut label = use_signal(String::new);
    let patch = use_memo(move || logic::add_hub_patch(&label.read(), &url.read(), &key.read()));
    rsx! { div { class: "modal-backdrop", onclick: move |_| onclose.call(()),
        div { class: "dialog", role: "dialog", "aria-modal": "true", "aria-label": "Add a CLIProxyAPI hub", tabindex: "-1",
            onclick: move |event| event.stop_propagation(),
            onkeydown: move |event| { if event.key() == Key::Escape { event.stop_propagation(); onclose.call(()); } },
            h2 { "Add a CLIProxyAPI hub" }
            p { class: "muted", "Show the quota of every account the hub pools, next to the providers on {environment}. The key stays on that server." }
            // Enter in a field clicks the submit button; its click saves, so the form itself never navigates.
            form { onsubmit: move |event| event.prevent_default(),
                label { class: "provider-field", div { strong { "Hub URL" } }
                    input { "aria-label": "Hub URL", placeholder: "https://hub.example.ts.net:8318", autocomplete: "off", spellcheck: "false", value: "{url}",
                        onmounted: move |event| { let element = event.data(); spawn(async move { let _ = element.set_focus(true).await; }); },
                        oninput: move |event| url.set(event.value()) } }
                label { class: "provider-field", div { strong { "Management key" } }
                    input { "aria-label": "Management key", r#type: "password", autocomplete: "off", value: "{key}", oninput: move |event| key.set(event.value()) } }
                label { class: "provider-field", div { strong { "Label (optional)" } }
                    input { "aria-label": "Label", placeholder: "Defaults to the hub's host name", value: "{label}", oninput: move |event| label.set(event.value()) } }
                div { class: "dialog-actions",
                    button { r#type: "button", "aria-label": "Cancel adding a hub", onclick: move |_| onclose.call(()), "Cancel" }
                    button { class: "primary", r#type: "submit", "aria-label": "Add hub to the list", disabled: patch.read().is_none(),
                        onclick: move |event| { event.prevent_default(); if let Some(patch) = patch.read().clone() { onsave.call(patch); } }, "Add hub" }
                }
            }
        }
    } }
}

/// How often the server re-checks providers in the background. It lives in the background
/// activity policy, which still decides whether a probe may run when the interval comes round.
#[component]
pub(crate) fn HealthInterval(state: Store<UiModel>, saving: bool, writable: bool, onsave: EventHandler<Value>) -> Element {
    let activity = use_memo(move || state.config().read()["settings"]["backgroundActivity"].clone());
    let known = state.config().read()["settings"]["backgroundActivity"].is_object();
    let seconds = interval::health_seconds(&activity.read());
    let changed = seconds != interval::health_preset_seconds(&activity.read());
    let off = saving || !writable || !known;
    rsx! {
        section { id: "provider-health-check-interval", class: "sc-section", "aria-label": "Health check interval",
            h2 { "Health check interval" }
            p { class: "muted", "Refresh provider status, versions, and models in the background. Set to 0 to disable. This interval is configured here, then the shared Background activity policy decides whether provider probes may run when the timer fires. Custom intervals appear as Advanced in General settings." }
            div { class: "fetch-stepper",
                button { r#type: "button", "aria-label": "Decrease provider health check interval", disabled: off || seconds == 0, onclick: move |_| onsave.call(interval::health_patch(&activity.peek(), Some(seconds.saturating_sub(interval::HEALTH_STEP_SECONDS)))), "\u{2212}" }
                input { r#type: "number", min: "0", step: "{interval::HEALTH_STEP_SECONDS}", "aria-label": "Provider health check interval in seconds", value: "{seconds}", disabled: off,
                    onchange: move |event| { if let Some(next) = interval::parse_seconds(&event.value()).filter(|next| *next != seconds) { onsave.call(interval::health_patch(&activity.peek(), Some(next))) } } }
                button { r#type: "button", "aria-label": "Increase provider health check interval", disabled: off, onclick: move |_| onsave.call(interval::health_patch(&activity.peek(), Some(seconds + interval::HEALTH_STEP_SECONDS))), "+" }
                span { class: "muted", "seconds" }
                if changed { button { r#type: "button", "aria-label": "Reset provider health check interval", disabled: off, onclick: move |_| onsave.call(interval::health_patch(&activity.peek(), None)), "Reset" } }
            }
        }
    }
}
