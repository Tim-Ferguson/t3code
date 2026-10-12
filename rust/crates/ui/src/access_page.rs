//! "Authorized clients" on the Connections page: the pairing links that can still be used and the clients signed in
//! to this server, with creating a link (what it may do, whom it is for), revoking one link or one client, and
//! revoking every client but this one. Port of the access section of `ConnectionsSettings.tsx`
//! (`AuthorizedClientsHeaderAction`, `PairingLinkListRow`, `ConnectedClientListRow`).
//!
//! The lists are read when the section opens and again after each change made here. A link's credential exists
//! only in the answer that created it, so only links made in this window can be copied or shared. "Share" opens the
//! original's panel: the endpoint to reach this machine at (when more than one is advertised), the link, and its QR code.
//! The endpoints are the desktop app's (`AdvertisedEndpoints`); a server reached over a network address that is not
//! loopback is the one endpoint of a browser, as in the original, where only the desktop knows more.
use crate::{
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
    settings_common::connected_allows,
};
use dioxus::prelude::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use t3_client::access as logic;
use t3_contracts::AuthEnvironmentScope;

/// The addresses this machine can be reached at, and the saved choice among them (`defaultAdvertisedEndpointKey`; nothing here
/// saves one). Provided by the desktop shell and by tests; a browser has none.
#[derive(Clone, Default, PartialEq)]
pub(crate) struct AdvertisedEndpoints {
    pub endpoints: Vec<logic::Endpoint>,
    pub default_key: Option<String>,
}

fn advertised() -> AdvertisedEndpoints {
    if let Some(provided) = try_consume_context::<AdvertisedEndpoints>() {
        return provided;
    }
    #[cfg(feature = "desktop")]
    {
        return crate::desktop_shell::advertised_endpoints();
    }
    #[allow(unreachable_code)]
    AdvertisedEndpoints::default()
}

#[derive(Clone, Default, PartialEq)]
struct Lists {
    links: Vec<Value>,
    clients: Vec<Value>,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[component]
pub(crate) fn Authorized(state: Store<UiModel>, transport: TransportHandle) -> Element {
    let can_read = connected_allows(state, AuthEnvironmentScope::AccessRead);
    let can_write = connected_allows(state, AuthEnvironmentScope::AccessWrite);
    if !can_read {
        return rsx! {};
    }
    let mut lists = use_signal(|| None::<Lists>);
    let mut failure = use_signal(|| None::<String>);
    let mut created = use_signal(BTreeMap::<String, String>::new);
    let mut busy = use_signal(|| None::<String>);
    let mut dialog = use_signal(|| false);
    let mut now = use_signal(now_ms);

    let reload = use_callback({
        let transport = transport.clone();
        move |()| {
            let transport = transport.clone();
            spawn(async move {
                let links = runtime::environment_json(&transport, state, None, logic::PAIRING_LINKS).await;
                let clients = runtime::environment_json(&transport, state, None, logic::CLIENTS).await;
                match (links, clients) {
                    (Ok(Value::Array(mut links)), Ok(Value::Array(mut clients))) => {
                        logic::sort_links(&mut links);
                        logic::sort_clients(&mut clients);
                        lists.set(Some(Lists { links, clients }));
                        failure.set(None);
                    }
                    (Err(message), _) | (_, Err(message)) => failure.set(Some(message)),
                    _ => failure.set(Some("The server answered with something this app does not read.".into())),
                }
            });
        }
    });
    // Who is signed in changes without this window's doing (a link used on another device, a client leaving): the
    // server says so, and the lists are read again.
    use_future({
        let transport = transport.clone();
        move || {
            let transport = transport.clone();
            async move {
                loop {
                    if *state.status().peek() == t3_client::connection::ConnectionStatus::Connected {
                        if let Ok(mut stream) = runtime::request_stream(&transport, state, "subscribeAuthAccess", json!({})) {
                            let mut first = true;
                            while let Some(Ok(_)) = stream.next().await {
                                // The first value is the snapshot the lists were just read for.
                                if !std::mem::take(&mut first) {
                                    reload.call(());
                                }
                            }
                        }
                    }
                    runtime::delay(2000).await;
                }
            }
        }
    });
    use_future(move || async move {
        reload.call(());
        // Expiry countdowns move on a slow tick; nothing repaints between ticks.
        loop {
            runtime::delay(15_000).await;
            now.set(now_ms());
        }
    });

    // One write at a time; the section is read again when it ends, whichever way.
    let change = use_callback({
        let transport = transport.clone();
        move |(key, path, payload): (String, &'static str, Value)| {
            if busy.peek().is_some() {
                return;
            }
            busy.set(Some(key));
            failure.set(None);
            let transport = transport.clone();
            spawn(async move {
                let answer = runtime::environment_json(&transport, state, Some(&payload), path).await;
                busy.set(None);
                if let Err(message) = answer {
                    failure.set(Some(message));
                }
                reload.call(());
            });
        }
    });

    let snapshot = lists.read().clone();
    let origin = runtime::connected_origin(&transport).unwrap_or_default();
    let advertised = advertised();
    let summary = snapshot.as_ref().map(|lists| logic::summarize(lists.clients.len(), lists.links.len()));
    let others = snapshot.as_ref().is_none_or(|lists| logic::can_revoke_others(&lists.clients));
    let revoking = busy.read().clone();
    rsx! {
        section { class: "access-section", "aria-label": "Authorized clients",
            div { class: "access-heading",
                div { h2 { "Authorized clients" } if let Some(summary) = summary { p { class: "muted", "{summary}" } } }
                div { class: "access-actions",
                    button { r#type: "button", class: "danger", "aria-label": "Revoke others", disabled: !can_write || revoking.is_some() || !others, onclick: move |_| change.call(("others".into(), logic::CLIENTS_REVOKE_OTHERS, json!({}))), if revoking.as_deref() == Some("others") { "Revoking\u{2026}" } else { "Revoke others" } }
                    button { r#type: "button", class: "primary", "aria-label": "Create link", disabled: !can_write, onclick: move |_| dialog.set(true), "Create link" }
                }
            }
            if let Some(message) = failure() { p { role: "alert", class: "publish-error", crate::error_text::ErrorText { text: message } } }
            match snapshot {
                None => rsx! { p { class: "muted", role: "status", "Loading authorized clients\u{2026}" } },
                Some(lists) => rsx! {
                    for link in lists.links.iter() {
                        {
                            let id = link["id"].as_str().unwrap_or_default().to_owned();
                            let credential = created.read().get(&id).cloned();
                            let url = credential.as_ref().map(|credential| logic::pairing_url(&origin, credential));
                            let label = link["label"].as_str().unwrap_or("Pairing link").to_owned();
                            let expires = logic::expires_in(logic::expires_at_ms(link), now());
                            let scopes = logic::scopes_of(link);
                            let (key, wire_id) = (format!("link:{id}"), id.clone());
                            rsx! {
                                article { key: "{id}", class: "access-row", "aria-label": "Pairing link {label}",
                                    div { class: "access-main",
                                        strong { "{label}" }
                                        p { class: "muted", "{expires} \u{b7} " span { title: "{scopes.join(\", \")}", "{scopes.len()} scopes" } }
                                        if let Some(url) = url.clone() {
                                            input { class: "access-url", readonly: true, "aria-label": "Pairing link for {label}", value: "{url}", onfocus: move |_| { dioxus::document::eval("document.activeElement?.select?.()"); } }
                                        } else { p { class: "muted", "Create a new link to share from this client." } }
                                    }
                                    if let Some(url) = url {
                                        button { r#type: "button", "aria-label": "Copy link for {label}", onclick: move |_| { let text = serde_json::to_string(&url).unwrap_or_default(); dioxus::document::eval(&format!("navigator.clipboard?.writeText({text})")); }, "Copy link" }
                                    }
                                    if let Some(credential) = credential.clone() {
                                        Share { key: "share:{id}", label: label.clone(), credential, origin: origin.clone(), advertised: advertised.clone() }
                                    }
                                    button { r#type: "button", class: "danger", "aria-label": "Revoke link {label}", disabled: !can_write || revoking.is_some(), onclick: move |_| change.call((key.clone(), logic::PAIRING_LINK_REVOKE, json!({"id": wire_id}))), if revoking.as_deref() == Some(&format!("link:{id}")) { "Revoking\u{2026}" } else { "Revoke" } }
                                }
                            }
                        }
                    }
                    for client in lists.clients.iter() {
                        {
                            let session = client["sessionId"].as_str().unwrap_or_default().to_owned();
                            let name = logic::client_label(client);
                            let details = logic::client_details(client).join(" \u{b7} ");
                            let scopes = logic::scopes_of(client);
                            let current = client["current"].as_bool().unwrap_or(false);
                            let live = current || client["connected"].as_bool().unwrap_or(false);
                            let (key, wire_id) = (format!("client:{session}"), session.clone());
                            rsx! {
                                article { key: "{session}", class: "access-row", "aria-label": "Client {name}",
                                    span { class: if live { "status-dot online" } else { "status-dot" }, title: if live { "Connected" } else { "Not connected" } }
                                    div { class: "access-main",
                                        strong { "{name}" if current { span { class: "access-badge", " This device" } } }
                                        p { class: "muted", if !details.is_empty() { "{details} \u{b7} " } span { title: "{scopes.join(\", \")}", "{scopes.len()} scopes" } }
                                    }
                                    if !current {
                                        button { r#type: "button", class: "danger", "aria-label": "Revoke client {name}", disabled: !can_write || revoking.is_some(), onclick: move |_| change.call((key.clone(), logic::CLIENT_REVOKE, json!({"sessionId": wire_id}))), if revoking.as_deref() == Some(&format!("client:{session}")) { "Revoking\u{2026}" } else { "Revoke" } }
                                    }
                                }
                            }
                        }
                    }
                },
            }
            if dialog() {
                CreateDialog { state, transport: transport.clone(), on_close: move |()| dialog.set(false), on_created: move |(id, credential): (String, String)| { created.write().insert(id, credential); dialog.set(false); reload.call(()); } }
            }
        }
    }
}

/// The pairing link's label and what it may do. The permissions offered are those this connection holds itself:
/// a client cannot hand on more than it has.
#[component]
fn CreateDialog(state: Store<UiModel>, transport: TransportHandle, on_close: EventHandler<()>, on_created: EventHandler<(String, String)>) -> Element {
    let delegatable: Vec<AuthEnvironmentScope> = {
        let destination = state.destination().read().clone();
        let model = state.environments();
        let environments = model.read();
        logic::SCOPE_OPTIONS.iter().map(|(scope, ..)| *scope).filter(|scope| destination.as_ref().is_some_and(|destination| environments.allows(destination, *scope))).collect()
    };
    let mut label = use_signal(String::new);
    let mut scopes = use_signal({
        let delegatable = delegatable.clone();
        move || logic::standard(&delegatable)
    });
    let mut pending = use_signal(|| false);
    let mut failure = use_signal(|| None::<String>);
    let selected: Vec<AuthEnvironmentScope> = scopes.read().iter().copied().filter(|scope| delegatable.contains(scope)).collect();
    let none = selected.is_empty();
    let manages_access = selected.contains(&AuthEnvironmentScope::AccessWrite);
    let create = {
        let selected = selected.clone();
        move |_| {
            if none || *pending.peek() {
                return;
            }
            pending.set(true);
            failure.set(None);
            let mut payload = json!({"scopes": selected.iter().map(|scope| logic::wire(*scope)).collect::<Vec<_>>()});
            if !label.peek().trim().is_empty() {
                payload["label"] = json!(label.peek().trim());
            }
            let transport = transport.clone();
            spawn(async move {
                let answer = runtime::environment_json(&transport, state, Some(&payload), logic::PAIRING_TOKEN).await;
                pending.set(false);
                match answer {
                    Ok(created) => match (created["id"].as_str(), created["credential"].as_str()) {
                        (Some(id), Some(credential)) => on_created.call((id.to_owned(), credential.to_owned())),
                        _ => failure.set(Some("The server did not return a pairing credential.".into())),
                    },
                    Err(message) => failure.set(Some(message)),
                }
            });
        }
    };
    rsx! {
        div { class: "modal-backdrop",
            div { class: "dialog access-dialog", role: "dialog", "aria-modal": "true", "aria-labelledby": "pairing-heading", tabindex: "-1",
                onmounted: move |event| { let element = event.data(); spawn(async move { let _ = element.set_focus(true).await; }); },
                onkeydown: move |event| { if event.key() == Key::Escape && !pending() { event.stop_propagation(); on_close.call(()); } },
                h2 { id: "pairing-heading", "Create pairing link" }
                p { class: "muted", "Generate a one-time link that another device can use to pair with this backend as an authorized client." }
                label { r#for: "pairing-label", "Client label (optional)" }
                input { id: "pairing-label", "aria-label": "Client label", placeholder: "e.g. Living room iPad", disabled: pending(), value: "{label}", oninput: move |event| label.set(event.value()) }
                div { class: "access-presets",
                    h3 { "Permissions" }
                    button { r#type: "button", "aria-label": "Read only", disabled: pending(), onclick: { let delegatable = delegatable.clone(); move |_| scopes.set(logic::READ_ONLY.iter().copied().filter(|scope| delegatable.contains(scope)).collect()) }, "Read only" }
                    button { r#type: "button", "aria-label": "Standard", disabled: pending(), onclick: { let delegatable = delegatable.clone(); move |_| scopes.set(logic::standard(&delegatable)) }, "Standard" }
                }
                ul { class: "access-scopes", "aria-label": "Permissions",
                    for (scope, title, description) in logic::SCOPE_OPTIONS.iter().filter(|(scope, ..)| delegatable.contains(scope)) {
                        {
                            let scope = *scope;
                            let checked = selected.contains(&scope);
                            rsx! { li { key: "{title}", label { class: "access-scope",
                                input { r#type: "checkbox", "aria-label": "{title}", checked, disabled: pending(), onchange: move |event| { let next = logic::toggle(&scopes.peek(), scope, event.checked()); scopes.set(next); } }
                                span { strong { "{title}" } br {} span { class: "muted", "{description}" } } } } }
                        }
                    }
                }
                if none { p { class: "publish-error", "Select at least one permission." } }
                else if manages_access { p { class: "commit-warning", "This client can create or revoke access for other devices." } }
                if let Some(message) = failure() { p { role: "alert", class: "publish-error", crate::error_text::ErrorText { text: message } } }
                div { class: "dialog-actions",
                    button { r#type: "button", disabled: pending(), "aria-label": "Cancel", onclick: move |_| on_close.call(()), "Cancel" }
                    button { class: "primary", r#type: "button", "aria-label": "Create", disabled: pending() || none, onclick: create, if pending() { "Creating\u{2026}" } else { "Create link" } }
                }
            }
        }
    }
}

/// The share panel of one link: which address to reach this machine at, the link to open there, and its QR code
/// (`PairingLinkListRow`'s panel). A link for this computer's own loopback address gets no code: the device that scanned
/// it would dial itself.
#[component]
fn Share(label: String, credential: String, origin: String, advertised: AdvertisedEndpoints) -> Element {
    let mut open = use_signal(|| false);
    let mut picked = use_signal(|| None::<String>);
    // The endpoint saved as the default (the original's `defaultAdvertisedEndpointKey`), read once from the browser's saved state; a pick
    // in this panel is the new default.
    let mut stored = use_signal(|| None::<String>);
    use_hook(move || spawn(async move { stored.set(read_default_key().await) }));
    let advertised = AdvertisedEndpoints { default_key: stored.read().clone().or(advertised.default_key.clone()), ..advertised };
    let options = logic::share_options(&advertised.endpoints, &credential);
    // `shareablePairingUrl`: the endpoint a link would use, else this connection's own address unless that is loopback.
    let shareable = logic::select_pairing_endpoint(&advertised.endpoints, advertised.default_key.as_deref())
        .map(|endpoint| endpoint.pairing_url(&credential))
        .or_else(|| (!origin.is_empty() && !logic::is_loopback_origin(&origin)).then(|| logic::pairing_url(&origin, &credential)));
    let Some(shareable) = shareable else { return rsx! {} };
    let selected = logic::select_share_option(&options, picked.read().as_deref(), advertised.default_key.as_deref()).cloned();
    let url = selected.as_ref().map_or(shareable, |option| option.url.clone());
    let drawable = selected.as_ref().is_none_or(|option| option.qr_shareable);
    let drawing = drawable.then(|| t3_client::qr::draw(&url, t3_client::qr::Level::Medium, 1)).flatten();
    let expanded = open();
    let (code, link) = (credential.clone(), url.clone());
    rsx! {
        button { r#type: "button", "aria-label": "Share pairing link {label}", "aria-expanded": "{expanded}", onclick: move |_| { let next = !open(); open.set(next); }, "Share" }
        if expanded {
            div { class: "access-share", role: "group", "aria-label": "Share {label}",
                if options.len() > 1 {
                    div { role: "radiogroup", "aria-label": "Endpoint the pairing QR code and URL use",
                        p { class: "muted", "Reach this machine via" }
                        for option in options.iter() {
                            {
                                let id = option.id.clone();
                                let preference = option.preference_key.clone();
                                let chosen = selected.as_ref().is_some_and(|selected| selected.id == option.id);
                                rsx! {
                                    button { key: "{option.id}", r#type: "button", role: "radio", "aria-checked": "{chosen}", "aria-label": "Reach via {option.label}", class: if chosen { "access-endpoint chosen" } else { "access-endpoint" },
                                        onclick: move |_| { picked.set(Some(id.clone())); stored.set(Some(preference.clone())); write_default_key(&preference); },
                                        strong { "{option.label}" } span { class: "muted", " {option.detail}" } }
                                }
                            }
                        }
                    }
                }
                div { class: "access-share-link",
                    code { "aria-label": "Pairing URL for {label}", "{url}" }
                    button { r#type: "button", "aria-label": "Copy pairing URL for {label}", onclick: move |_| { let link = link.clone(); spawn(async move { crate::settings_common::copy_text(&link).await; }); }, "Copy link" }
                }
                button { r#type: "button", "aria-label": "Copy code only for {label}", onclick: move |_| { let code = code.clone(); spawn(async move { crate::settings_common::copy_text(&code).await; }); }, "Copy code only" }
                if let Some(drawing) = drawing {
                    svg { class: "access-qr", xmlns: "http://www.w3.org/2000/svg", view_box: "0 0 {drawing.view} {drawing.view}", width: "168", height: "168", shape_rendering: "crispEdges", role: "img", "aria-label": "Pairing link \u{2014} scan to open on another device",
                        rect { width: "{drawing.view}", height: "{drawing.view}", fill: "#fff" }
                        path { d: "{drawing.path}", fill: "#000" }
                    }
                } else {
                    p { class: "muted", role: "note", "No QR for this endpoint. Another device scanning a loopback link would dial itself; copy the URL for use on this machine instead." }
                }
            }
        }
    }
}

pub(crate) async fn read_default_key() -> Option<String> {
    let key = serde_json::to_string(logic::UI_STATE_KEY).expect("key");
    let raw = document::eval(&format!("try{{return window.localStorage.getItem({key});}}catch{{return null;}}")).join::<Option<String>>().await.ok().flatten();
    logic::stored_default_key(raw.as_deref())
}

/// Saves the endpoint as the default, in the record the original's interface state shares.
pub(crate) fn write_default_key(preference: &str) {
    let (key, preference) = (serde_json::to_string(logic::UI_STATE_KEY).expect("key"), serde_json::to_string(preference).expect("preference"));
    let _ = document::eval(&format!("try{{const raw=window.localStorage.getItem({key});let state={{}};try{{const parsed=JSON.parse(raw);if(parsed&&typeof parsed==='object'&&!Array.isArray(parsed))state=parsed;}}catch{{}}state.defaultAdvertisedEndpointKey={preference};window.localStorage.setItem({key},JSON.stringify(state));}}catch{{}}"));
}
