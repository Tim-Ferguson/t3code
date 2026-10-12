//! The browser defaults of Settings, Integrations (`BrowserZoomSetting`, `BrowserAppearanceSetting`, `BrowserLinkTargetSetting` and
//! `BrowserProfilesSetting` of `IntegrationsSettings.tsx`): the zoom and color scheme a new tab starts with, where a link in the chat
//! opens, and the profiles that keep cookies and logins apart. They are this device's own (the client settings). Clearing a profile's
//! data, or removing the profile, is asked of every connected environment that hosts browser tabs (`preview.clearProfile`).
//! Not ported: importing cookies from another browser on this computer, which is a desktop-shell service.
use crate::{
    fleet,
    notice::{self, Level},
    runtime::{TransportHandle, UiModel, UiModelStoreExt},
    settings_common::Confirm,
    settings_rows,
};
use dioxus::prelude::*;
use serde_json::{Value, json};
use t3_client::{browser_profiles as logic, connection::ConnectionStatus, preview};
use t3_contracts::{BrowserLinkTarget, BrowserProfileKind, EnvironmentId, PreviewAppearancePreference};

fn percent(zoom: f64) -> String {
    format!("{}%", (zoom * 100.0).round())
}
fn wire<T: serde::Serialize>(value: T) -> String {
    serde_json::to_value(value).ok().and_then(|value| value.as_str().map(str::to_owned)).unwrap_or_default()
}

/// The connected environments whose servers host browser tabs: where a profile's data is kept.
fn hosting(state: Store<UiModel>) -> Vec<EnvironmentId> {
    let model = state.peek();
    model
        .environments
        .records
        .keys()
        .filter(|id| {
            fleet::status(state, id) == ConnectionStatus::Connected
                && (if model.destination.as_ref() == Some(*id) { model.typed_config.as_ref() } else { model.parked.get(*id).and_then(|slice| slice.typed_config.as_ref()) })
                    .is_some_and(|config| config.environment.capabilities.server_browser == Some(true))
        })
        .cloned()
        .collect()
}

/// Asks every hosting environment to forget a profile's cookies and cache (and close its tabs).
async fn clear_everywhere(transport: TransportHandle, state: Store<UiModel>, profile_id: String) -> Result<(), String> {
    let environments = hosting(state);
    if environments.is_empty() {
        return Err("You're not connected to a server yet.".to_owned());
    }
    let asks = environments.iter().map(|environment| {
        let link = transport.link_of(environment);
        let profile_id = profile_id.clone();
        async move {
            match link {
                Some(link) => crate::runtime::request_value_reported(link, state, "preview.clearProfile", json!({"profileId": profile_id})).await.map(|_| ()),
                None => Err("The environment was removed.".to_owned()),
            }
        }
    });
    futures_util::future::join_all(asks).await.into_iter().collect()
}

#[derive(Clone, PartialEq)]
enum Pending {
    Clear { id: String, name: String },
    Remove { id: String, name: String },
}

#[component]
pub(crate) fn Rows(state: Store<UiModel>, transport: TransportHandle) -> Element {
    let settings = state.client_settings().read().clone();
    let mut pending = use_signal(|| None::<Pending>);
    let mut working = use_signal(|| false);
    let zoom = settings.browser_default_zoom_factor.0;
    let appearance = wire(settings.browser_default_appearance);
    let link_target = wire(settings.browser_link_target);
    let profiles = t3_contracts::resolve_browser_profiles(&settings.browser_profiles);
    let listed: Vec<_> = profiles.iter().filter(|profile| profile.kind != BrowserProfileKind::Incognito).cloned().collect();
    let default_id = listed.iter().find(|profile| profile.id == settings.browser_default_profile_id).map_or_else(|| logic::DEFAULT_ID.to_owned(), |profile| profile.id.as_str().to_owned());
    let own: Vec<Value> = settings.browser_profiles.iter().filter_map(|profile| serde_json::to_value(profile).ok()).collect();
    let taken: Vec<String> = profiles.iter().map(|profile| profile.name.0.as_str().to_owned()).collect();
    let at_limit = own.len() >= logic::MAX_COUNT;
    let patch = move |change: Value| settings_rows::patch(state, change);

    let clear = {
        let transport = transport.clone();
        move |id: String, name: String| {
            let transport = transport.clone();
            spawn(async move {
                match clear_everywhere(transport, state, id).await {
                    Ok(()) => notice::push(state, Level::Info, &logic::cleared_title(&name), ""),
                    Err(message) => notice::push(state, Level::Error, &format!("Could not clear {name}\u{2019}s data"), &message),
                }
            });
        }
    };
    let remove = {
        let (transport, own, default_id) = (transport.clone(), own.clone(), default_id.clone());
        move |id: String| {
            let (transport, own, default_id) = (transport.clone(), own.clone(), default_id.clone());
            working.set(true);
            spawn(async move {
                // The data goes first, or a removed profile's cookies would stay on disk with nothing pointing at them.
                match clear_everywhere(transport, state, id.clone()).await {
                    Ok(()) => {
                        patch(logic::removal(&own, &default_id, &id));
                        pending.set(None);
                    }
                    Err(message) => notice::push(state, Level::Error, "Profile data could not be deleted", &format!("{message} Try again.")),
                }
                working.set(false);
            });
        }
    };
    let current = pending.read().clone();
    rsx! {
        div { class: "setting-row", "data-setting": "browser-default-zoom",
            div { class: "font-description", strong { "Default browser zoom" } p { class: "muted", "Page zoom applied to new browser tabs." } }
            div { class: "font-controls",
                select { "aria-label": "Default browser zoom", value: "{zoom}", onchange: move |event| { if let Ok(level) = event.value().parse::<f64>() { if preview::ZOOM_LEVELS.contains(&level) { patch(json!({"browserDefaultZoomFactor": level})); } } },
                    for level in preview::ZOOM_LEVELS { option { key: "{level}", value: "{level}", selected: level == zoom, "{percent(level)}" } }
                }
                if zoom != preview::DEFAULT_ZOOM { button { r#type: "button", "aria-label": "Reset default browser zoom", onclick: move |_| patch(json!({"browserDefaultZoomFactor": preview::DEFAULT_ZOOM})), "Reset" } }
            }
        }
        div { class: "setting-row", "data-setting": "browser-default-appearance",
            div { class: "font-description", strong { "Default browser appearance" } p { class: "muted", "The color scheme pages are told to prefer. System follows your OS setting." } }
            div { class: "font-controls",
                select { "aria-label": "Default browser appearance", value: "{appearance}", onchange: move |event| patch(json!({"browserDefaultAppearance": event.value()})),
                    for (value, label) in [(PreviewAppearancePreference::System, "System"), (PreviewAppearancePreference::Light, "Light"), (PreviewAppearancePreference::Dark, "Dark")] { option { key: "{wire(value)}", value: "{wire(value)}", selected: wire(value) == appearance, "{label}" } }
                }
                if appearance != "system" { button { r#type: "button", "aria-label": "Reset default browser appearance", onclick: move |_| patch(json!({"browserDefaultAppearance": "system"})), "Reset" } }
            }
        }
        div { class: "setting-row", "data-setting": "browser-link-target",
            div { class: "font-description", strong { "Open links in" } p { class: "muted", "Where links in the chat open. Hold \u{2318} or Ctrl while clicking a link to open it in your default browser either way." } }
            div { class: "font-controls",
                select { "aria-label": "Open links in", value: "{link_target}", onchange: move |event| patch(json!({"browserLinkTarget": event.value()})),
                    for (value, label) in [(BrowserLinkTarget::System, "Your default browser"), (BrowserLinkTarget::App, "T3 Code")] { option { key: "{wire(value)}", value: "{wire(value)}", selected: wire(value) == link_target, "{label}" } }
                }
                if link_target != "system" { button { r#type: "button", "aria-label": "Reset link target", onclick: move |_| patch(json!({"browserLinkTarget": "system"})), "Reset" } }
            }
        }
        div { class: "setting-row", id: "browser-profiles", "data-setting": "browser-profiles", role: "group", "aria-label": "Browser profiles",
            div { class: "font-description", strong { "Browser profiles" } p { class: "muted", "Profiles separate cookies and logins. Incognito data is cleared when the app closes." } }
            div { class: "font-controls",
                button { r#type: "button", "aria-label": "Add profile", disabled: at_limit, title: if at_limit { "You\u{2019}ve reached the profile limit" } else { "" },
                    onclick: { let (own, taken) = (own.clone(), taken.clone()); move |_| { if let Some(next) = logic::added(&own, &format!("profile-{}", uuid::Uuid::new_v4()), &taken) { patch(json!({"browserProfiles": next})); } } }, "Add profile" }
            }
            ul { class: "profile-list",
                for profile in listed {
                    {
                        let (id, name) = (profile.id.as_str().to_owned(), profile.name.0.as_str().to_owned());
                        let built_in = id == logic::DEFAULT_ID;
                        let is_default = id == default_id;
                        rsx! { li { key: "{id}", class: "profile-row",
                            if built_in { span { class: "profile-name", "{name}" } }
                            else {
                                input { class: "profile-name", "aria-label": "Rename {name}", maxlength: "{logic::NAME_MAX_LENGTH}", value: "{name}",
                                    onchange: { let (own, id) = (own.clone(), id.clone()); move |event| { if let Some(next) = logic::renamed(&own, &id, &event.value()) { patch(json!({"browserProfiles": next})); } } } }
                            }
                            if is_default { span { class: "profile-default", title: "Default browser profile", "Default" } }
                            div { class: "profile-actions",
                                button { r#type: "button", "aria-label": "Set {name} as default", disabled: is_default, onclick: { let id = id.clone(); move |_| patch(json!({"browserDefaultProfileId": id})) }, "Set as default" }
                                button { r#type: "button", "aria-label": "Clear cookies and cache of {name}", onclick: { let (id, name) = (id.clone(), name.clone()); move |_| pending.set(Some(Pending::Clear { id: id.clone(), name: name.clone() })) }, "Clear cookies and cache" }
                                if !built_in { button { class: "destructive", r#type: "button", "aria-label": "Remove {name}", onclick: { let (id, name) = (id.clone(), name.clone()); move |_| pending.set(Some(Pending::Remove { id: id.clone(), name: name.clone() })) }, "Remove profile and data" } }
                            }
                        } }
                    }
                }
            }
        }
        match current {
            Some(Pending::Clear { id, name }) => rsx! {
                Confirm { label: "clearing {name}", title: format!("Clear \u{201c}{name}\u{201d}\u{2019}s cookies and cache?"), body: "You are signed out of its sites. Server browser tabs open in this profile close now.", confirm: "Clear data", busy: false,
                    oncancel: move |()| pending.set(None),
                    onconfirm: { let clear = clear.clone(); move |()| { pending.set(None); clear(id.clone(), name.clone()); } } }
            },
            Some(Pending::Remove { id, name }) => rsx! {
                Confirm { label: "removing {name}", title: format!("Remove \u{201c}{name}\u{201d}?"), body: "Its cookies and logins are deleted. Server browser tabs open in this profile close now.", confirm: "Remove profile", busy: *working.read(),
                    oncancel: move |()| pending.set(None),
                    onconfirm: { let mut remove = remove.clone(); move |()| remove(id.clone()) } }
            },
            None => rsx! {},
        }
    }
}
