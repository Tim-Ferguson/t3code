//! "GitHub sharing" of the Connections page (`GitHubRoutingSettings.tsx`): for each machine, how much of its signed-in GitHub account the
//! other machines may use through this client. The grants are this device's own (`connection_store`), tied to each machine's saved
//! addresses; what they allow is `pull_request_routing`. Drawn when there are two or more machines.
use crate::{
    connection_store,
    notice::{self, Level},
    runtime::{UiModel, UiModelStoreExt},
};
use dioxus::prelude::*;
use t3_client::github_routing::{self as rules, Permission};
use t3_contracts::EnvironmentId;

#[component]
pub(crate) fn Section(state: Store<UiModel>) -> Element {
    // The grants follow the saved connections and the machines this run has seen.
    let _ = state.saved_revision().read();
    let machines: Vec<(EnvironmentId, String)> = state.environments().read().records.values().map(|record| (record.id.clone(), record.label.clone())).collect();
    if machines.len() < 2 {
        return rsx! {};
    }
    let granted: Vec<(String, Permission)> = machines.iter().map(|(id, label)| (label.clone(), connection_store::github_permission(state, id))).collect();
    let summary = rules::summarize(&granted).unwrap_or_else(|| "Off".to_owned());
    rsx! {
        section { class: "setting-row", id: "github-routing", "data-setting": "github-routing", "aria-label": "GitHub sharing",
            h3 { "GitHub sharing" }
            span { class: "muted", "{summary}" }
            p { class: "muted", "Machines you trust here can read PR data through each other\u{2019}s GitHub access. Enable both machines. Read and act may use broader permissions than the machine that owns them. This applies only to this device." }
            for ((id, label), (_, current)) in machines.into_iter().zip(granted) {
                {
                    let key_known = connection_store::github_key(state, &id).is_some();
                    rsx! {
                        label { key: "{id}", class: "setting-row", "{label}"
                            select { "aria-label": "{label} GitHub routing", disabled: !key_known, value: "{current.wire()}",
                                onchange: {
                                    let id = id.clone();
                                    move |event| {
                                        let Some(permission) = Permission::parse(&event.value()) else { return };
                                        if let Err(cause) = connection_store::set_github_permission(state, &id, permission) {
                                            notice::push(state, Level::Error, "Could not save GitHub routing permission", cause);
                                        }
                                    }
                                },
                                for permission in Permission::ALL { option { key: "{permission.wire()}", value: "{permission.wire()}", selected: permission == current, "{permission.label()}" } }
                            }
                        }
                    }
                }
            }
        }
    }
}
