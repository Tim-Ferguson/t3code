//! Load balancing between environments (`LoadBalancingSettings.tsx`, `useLoadBalancedEnvironment.ts`): a Connections section where the
//! person turns balancing on for this client and says how often each machine should receive new threads, and "Auto balance" in the
//! new-thread form's "Run on" choice, which starts the thread on the machine with the most free CPU and memory, weighted by those
//! preferences. The machines are asked for their resources (`server.getHostResources`) when the choice is made, each on its own
//! connection, and a machine that does not answer in time or is busy is passed over.
use crate::{
    fleet,
    notice::{self, Level},
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
    settings_rows,
};
use dioxus::prelude::*;
use serde_json::json;
use t3_client::load_balancing as logic;
use t3_contracts::{EnvironmentId, HostResourcesSnapshot};

fn weight_of(state: Store<UiModel>, environment: &str) -> Option<u8> {
    state.client_settings().read().load_balancing_weights.iter().find(|(key, _)| key.as_str() == environment).map(|(_, weight)| weight.0 as u8)
}

/// The folded section under the saved connections: its switch turns balancing on, and each saved machine has a row for how often it
/// should receive new threads. Drawn when there are two or more machines, since one has nothing to balance against.
#[component]
pub(crate) fn Section(state: Store<UiModel>) -> Element {
    let machines: Vec<(EnvironmentId, String)> = state.environments().read().records.values().map(|record| (record.id.clone(), record.label.clone())).collect();
    if machines.len() < 2 {
        return rsx! {};
    }
    let enabled = state.client_settings().read().load_balancing_enabled;
    let summary = if enabled { logic::summarize(machines.iter().map(|(id, label)| (id.as_str(), label.as_str())), |id| weight_of(state, id)) } else { Some("Off".to_owned()) };
    rsx! {
        section { class: "setting-row", id: "load-balancing", "data-setting": "load-balancing", "aria-label": "Load balancing",
            label { class: "switch-row",
                input { r#type: "checkbox", role: "switch", "aria-label": "Automatically balance load", checked: enabled,
                    onchange: move |event| settings_rows::patch(state, json!({"loadBalancingEnabled": event.checked()})) }
                " Load balancing" }
            if let Some(summary) = summary { span { class: "muted", "{summary}" } }
            p { class: "muted", "New threads in shared projects start on the machine with the most free CPU and memory, weighted by each machine\u{2019}s preference." }
            for (id, label) in machines {
                {
                    let current = logic::preference_for_weight(weight_of(state, id.as_str()));
                    let key = id.to_string();
                    rsx! {
                        label { key: "{id}", class: "setting-row", "{label}"
                            select { "aria-label": "{label} load preference", disabled: !enabled, value: "{current}",
                                onchange: move |event| {
                                    let Ok(chosen) = event.value().parse::<u8>() else { return };
                                    // The whole map is written, as the original writes it.
                                    let mut weights: serde_json::Map<String, serde_json::Value> = state.client_settings().peek().load_balancing_weights.iter().map(|(id, weight)| (id.to_string(), json!(weight.0))).collect();
                                    weights.insert(key.clone(), json!(chosen));
                                    settings_rows::patch(state, json!({"loadBalancingWeights": weights}));
                                },
                                for (value, name) in logic::PREFERENCES { option { key: "{value}", value: "{value}", selected: value == current, "{name}" } }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// How long a machine has to report its resources before it is passed over.
const WAIT_MS: u64 = 5_000;

/// The machine to start on, among `candidates`: each is asked for its resources on its own connection and the best fresh answer wins.
/// None when no machine qualifies (none answered in time, all are busy, or all are set to "Manual only").
pub(crate) async fn choose(transport: &TransportHandle, state: Store<UiModel>, candidates: &[EnvironmentId]) -> Option<EnvironmentId> {
    let asks = candidates.iter().map(|environment| {
        let link = fleet::link(transport, environment);
        async move {
            let answer = futures_util::future::select(
                Box::pin(runtime::request_value_reported(link, state, "server.getHostResources", json!({}))),
                Box::pin(runtime::delay(WAIT_MS)),
            )
            .await;
            match answer {
                futures_util::future::Either::Left((Ok(value), _)) => serde_json::from_value::<HostResourcesSnapshot>(value).ok().map(|resources| (resources, now())),
                _ => None,
            }
        }
    });
    let answers = futures_util::future::join_all(asks).await;
    let weights: Vec<f64> = candidates.iter().map(|environment| f64::from(weight_of(state, environment.as_str()).unwrap_or(logic::DEFAULT_WEIGHT))).collect();
    let pool: Vec<logic::Candidate<'_>> = candidates
        .iter()
        .zip(&answers)
        .zip(&weights)
        .map(|((environment, answer), weight)| logic::Candidate { environment: environment.as_str(), resources: answer.as_ref().map(|(resources, _)| resources), received_at: answer.as_ref().map(|(_, at)| *at), weight: *weight })
        .collect();
    logic::choose(&pool, now()).and_then(|chosen| candidates.iter().find(|environment| environment.as_str() == chosen).cloned())
}

fn now() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

/// "Auto balance" in the form's machine choice: starts the thread's form on the machine that is best for it now, and says which, or
/// that none could be chosen. The choice is made once, when asked; the form is then an ordinary form on that machine.
pub(crate) async fn auto_balance(transport: TransportHandle, state: Store<UiModel>, hosts: Vec<crate::new_thread::Host>) {
    let candidates: Vec<EnvironmentId> = hosts.iter().map(|host| host.environment.clone()).collect();
    match choose(&transport, state, &candidates).await {
        Some(chosen) => {
            if let Some(host) = hosts.iter().find(|host| host.environment == chosen) {
                crate::new_thread::retarget(&transport, state, host);
                notice::push(state, Level::Info, "Auto balance", format!("Starting on {}.", host.label));
            }
        }
        None => notice::push(state, Level::Warning, "Auto balance unavailable", "No machine reported enough free CPU and memory. Choose one."),
    }
}
