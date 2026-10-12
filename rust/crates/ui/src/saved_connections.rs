//! The "Saved connections" list of the Connections page: every environment this app knows, how each is connected right now, the
//! routes it can be reached over, and what can be done with each (`ConnectionsSettings.tsx`). Every connected environment stays
//! connected; "Open" makes one the window's. A saved environment that has a direct route and its token connects with them, without
//! pairing again; switching one off disconnects it and keeps it and its token, and nothing connects it by itself; forgetting one
//! removes the routes, the profiles and the token. The token is read when "Connect" is clicked and goes straight to the
//! connection: it is not shown, held in a signal, or put in the address.
use crate::runtime::{self, TransportHandle, UiModel, UiModelStoreExt, View};
use dioxus::prelude::*;
use t3_client::connection::ConnectionStatus;

/// One row of the list.
struct Row {
    id: t3_contracts::EnvironmentId,
    label: String,
    /// The routes, preferred first, as "LAN · http://192.168.1.4:3773/".
    routes: Vec<String>,
    /// The address "Use address" fills the form with: the preferred direct route's, else the last one the environment was reached at.
    address: String,
    saved: bool,
    enabled: bool,
    /// Whether it has a direct route and a token to connect with.
    connectable: bool,
    /// How it is connected now; none when it is not (no connection exists).
    status: Option<ConnectionStatus>,
    /// Whether it is the environment the window is on.
    open: bool,
}

fn status_words(status: &ConnectionStatus) -> String {
    match status {
        ConnectionStatus::Connected => "Connected".into(),
        ConnectionStatus::Connecting => "Connecting\u{2026}".into(),
        ConnectionStatus::Interrupted(_) | ConnectionStatus::Disconnected => "Disconnected. Reconnecting\u{2026}".into(),
        ConnectionStatus::Blocked(reason) => format!("Needs attention: {reason}"),
    }
}

fn rows(state: Store<UiModel>) -> Vec<Row> {
    let model = state.peek();
    model
        .environments
        .records
        .values()
        .map(|record| {
            let entry = model.saved.entry(&record.id);
            let routes: Vec<String> = entry
                .iter()
                .flat_map(|entry| entry.routes.iter())
                .map(|route| match route.address() {
                    Some(address) => format!("{} \u{b7} {address}", route.label()),
                    None => route.label(),
                })
                .collect();
            let direct = entry.as_ref().and_then(|entry| entry.routes.iter().find(|route| route.http_base_url().is_some()));
            let connectable = direct.is_some_and(|route| route.target.connection_id().is_some_and(|id| model.saved.token(id).is_some()));
            Row {
                id: record.id.clone(),
                label: record.label.clone(),
                routes: if routes.is_empty() { vec![record.address.clone()] } else { routes },
                address: direct.and_then(|route| route.http_base_url().map(str::to_owned)).unwrap_or_else(|| record.address.clone()),
                saved: entry.is_some(),
                enabled: entry.as_ref().is_none_or(|entry| entry.enabled),
                connectable,
                status: model.live.get(&record.id).cloned(),
                open: model.destination.as_ref() == Some(&record.id),
            }
        })
        .collect()
}

#[component]
pub(crate) fn List(state: Store<UiModel>, transport: TransportHandle, on_use_address: EventHandler<String>) -> Element {
    // The list follows the saved connections, the environments this run has seen and how each is connected.
    let _ = state.saved_revision().read();
    let _ = state.environments().read();
    let _ = state.live().read();
    let _ = state.destination().read();
    let rows = rows(state);
    if rows.is_empty() {
        return rsx! {};
    }
    rsx! {
        h2 { "Saved connections" }
        for row in rows {
            article { class: "connection-row", key: "{row.id}",
                div {
                    strong { "{row.label}" }
                    for route in row.routes.iter() { p { class: "muted", "{route}" } }
                    if let Some(status) = row.status.as_ref() { p { class: "muted", role: "status", "{status_words(status)}" } }
                    if row.open { p { class: "muted", "The window is on this environment." } }
                    if row.saved && !row.enabled { p { class: "muted", "Switched off. It stays saved and does not connect." } }
                }
                if row.status.is_some() && !row.open {
                    button { "aria-label": "Open {row.label}", title: "Show this environment; it stays connected either way",
                        onclick: {
                            let (transport, id) = (transport.clone(), row.id.clone());
                            move |_| {
                                if runtime::switch_to_environment(&transport, state, &id) {
                                    state.view().set(View::Chat);
                                }
                            }
                        },
                        "Open" }
                }
                if row.status.is_none() && row.connectable {
                    button { "aria-label": "Connect {row.label}", disabled: !row.enabled, title: "Connect with the saved sign-in; no pairing needed",
                        onclick: {
                            let (transport, id) = (transport.clone(), row.id.clone());
                            move |_| {
                                let Some((address, token)) = saved_route(state, &id) else { return };
                                let transport = transport.clone();
                                // The token goes from the store to the connection and nowhere else.
                                spawn(async move { runtime::connect(transport, state, address, token).await });
                            }
                        },
                        "Connect" }
                }
                if matches!(row.status, Some(ConnectionStatus::Interrupted(_))) {
                    button { "aria-label": "Retry {row.label}", title: "Try to connect now instead of waiting",
                        onclick: { let (transport, id) = (transport.clone(), row.id.clone()); move |_| runtime::retry_now(&transport, &id) },
                        "Retry now" }
                }
                if row.status.is_some() {
                    button { "aria-label": "Disconnect {row.label}", title: "End this connection; the environment stays saved",
                        onclick: { let (transport, id) = (transport.clone(), row.id.clone()); move |_| runtime::disconnect_environment(&transport, state, &id) },
                        "Disconnect" }
                }
                button { "aria-label": "Use address of {row.label}", onclick: { let address = row.address.clone(); move |_| on_use_address.call(address.clone()) }, "Use address" }
                if row.saved {
                    button { "aria-label": if row.enabled { "Switch off {row.label}" } else { "Switch on {row.label}" },
                        onclick: {
                            let (transport, id) = (transport.clone(), row.id.clone());
                            let enabled = row.enabled;
                            move |_| {
                                if enabled {
                                    runtime::disconnect_environment(&transport, state, &id);
                                }
                                crate::connection_store::set_enabled(state, &id, !enabled);
                            }
                        },
                        if row.enabled { "Switch off" } else { "Switch on" } }
                }
                button { "aria-label": "Forget {row.label}",
                    onclick: {
                        let (transport, id) = (transport.clone(), row.id.clone());
                        move |_| {
                            runtime::forget_environment(&transport, state, &id);
                            crate::connection_store::forget(state, &id);
                        }
                    },
                    "Forget" }
            }
        }
    }
}

/// The address and token of an environment's preferred direct route.
fn saved_route(state: Store<UiModel>, id: &t3_contracts::EnvironmentId) -> Option<(String, String)> {
    let saved = state.peek().saved.clone();
    let entry = saved.entry(id).filter(|entry| entry.enabled)?;
    let route = entry.routes.iter().find(|route| route.http_base_url().is_some())?;
    Some((route.http_base_url()?.to_owned(), saved.token(route.target.connection_id()?)?))
}
