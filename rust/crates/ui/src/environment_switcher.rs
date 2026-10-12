//! The connected environments at the top of the sidebar, each with how it is connected, when there is more than one: the window is on
//! one, and the others are a click away with everything they had (`links`). It lists connections, not threads; what the sidebar lists
//! beneath it is the open environment's.
use crate::runtime::{self, TransportHandle, UiModel, UiModelStoreExt, View};
use dioxus::prelude::*;
use t3_client::connection::ConnectionStatus;

fn mark(status: &ConnectionStatus) -> (&'static str, &'static str) {
    match status {
        ConnectionStatus::Connected => ("status-dot online", "Connected"),
        ConnectionStatus::Connecting => ("status-dot busy", "Connecting\u{2026}"),
        ConnectionStatus::Interrupted(_) | ConnectionStatus::Disconnected => ("status-dot lost", "Disconnected"),
        ConnectionStatus::Blocked(_) => ("status-dot blocked", "Needs attention"),
    }
}

#[component]
pub(crate) fn Switcher(state: Store<UiModel>, transport: TransportHandle) -> Element {
    let _ = state.live().read();
    let _ = state.destination().read();
    let _ = state.environments().read();
    let rows: Vec<_> = {
        let model = state.peek();
        let named = |id: &t3_contracts::EnvironmentId| model.environments.records.get(id).map_or_else(|| id.to_string(), |record| record.label.clone());
        let labels: Vec<String> = model.live.keys().map(named).collect();
        model
            .live
            .iter()
            .map(|(id, status)| {
                let mut label = named(id);
                // Two machines can be called the same (a host name is what the server calls itself): the address tells them apart.
                if labels.iter().filter(|other| **other == label).count() > 1 {
                    if let Some(record) = model.environments.records.get(id) {
                        label = format!("{label} ({})", record.address.trim_start_matches("http://").trim_start_matches("https://").trim_end_matches('/'));
                    }
                }
                (id.clone(), label, status.clone(), model.destination.as_ref() == Some(id))
            })
            .collect()
    };
    // One environment is the window's whole world; the list is for when there are others.
    if rows.len() < 2 {
        return rsx! {};
    }
    rsx! {
        nav { class: "environment-switcher", "aria-label": "Connected environments",
            for (id, label, status, open) in rows {
                {
                    let (dot, words) = mark(&status);
                    rsx! {
                        button { key: "{id}", class: if open { "environment-choice open" } else { "environment-choice" }, "aria-label": "Switch to {label}", "aria-current": if open { "true" } else { "false" }, title: "{label}: {words}",
                            onclick: {
                                let (transport, id) = (transport.clone(), id.clone());
                                move |_| {
                                    if runtime::switch_to_environment(&transport, state, &id) {
                                        state.view().set(View::Chat);
                                    }
                                }
                            },
                            span { class: "{dot}", "aria-hidden": "true" }
                            span { class: "environment-name", "{label}" }
                        }
                    }
                }
            }
        }
    }
}
