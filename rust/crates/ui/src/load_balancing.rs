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

/// How the choice of "Auto balance" stands (`autoEnvironmentLabel` of `ChatView.tsx`): the machines are being asked, none could be
/// chosen, or it is done.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Phase {
    #[default]
    Idle,
    Checking,
    Unavailable,
}

/// The form the person gave "Auto balance" (it is on the machine the balancing chose), and how the choice stands. The form stays
/// automatic after it is on that machine: the picker keeps saying "Auto balance" until another machine is chosen.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Auto {
    pub draft: Option<(EnvironmentId, String)>,
    pub phase: Phase,
}

/// What the picker's automatic entry says (`autoEnvironmentLabel`).
pub(crate) fn auto_label(phase: Phase) -> &'static str {
    match phase {
        Phase::Idle => "Auto balance",
        Phase::Checking => "Checking machines\u{2026}",
        Phase::Unavailable => "Auto balance unavailable",
    }
}

/// Whether the form on screen is the one that has been given "Auto balance".
pub(crate) fn is_automatic(state: Store<UiModel>) -> bool {
    let (destination, draft) = (state.destination().read().clone(), state.active_draft().read().clone());
    match (destination, draft) {
        (Some(destination), Some(draft)) => state.auto_balance().read().draft == Some((destination, draft)),
        _ => false,
    }
}

/// The form's machine is chosen by the person: it is no longer automatic.
pub(crate) fn forget_automatic(state: Store<UiModel>) {
    let mut auto = state.auto_balance();
    if auto.peek().draft.is_some() || auto.peek().phase != Phase::Idle {
        *auto.write() = Auto::default();
    }
}

/// "Auto balance" in the form's machine choice: starts the thread's form on the machine that is best for it now, and says which, or
/// that none could be chosen. The form stays automatic (`environmentSelection: "auto"`) on the machine chosen, while the machines are
/// being asked the picker says so, and when none can be chosen it says that and the form stays where it was.
pub(crate) async fn auto_balance(transport: TransportHandle, state: Store<UiModel>, hosts: Vec<crate::new_thread::Host>) {
    let candidates: Vec<EnvironmentId> = hosts.iter().map(|host| host.environment.clone()).collect();
    let mut auto = state.auto_balance();
    auto.write().phase = Phase::Checking;
    let owner = runtime::response_owner(&transport, state);
    let chosen = choose(&transport, state, &candidates).await;
    // The window moved on to another connection while the machines answered: this is no longer its question.
    if owner.is_none() || owner != runtime::response_owner(&transport, state) {
        auto.write().phase = Phase::Idle;
        return;
    }
    match chosen {
        Some(chosen) => {
            if let Some(host) = hosts.iter().find(|host| host.environment == chosen) {
                crate::new_thread::retarget(&transport, state, host);
                let (destination, draft) = (state.destination().peek().clone(), state.active_draft().peek().clone());
                if let (Some(destination), Some(draft)) = (destination, draft) {
                    *auto.write() = Auto { draft: Some((destination, draft)), phase: Phase::Idle };
                }
                notice::push(state, Level::Info, "Auto balance", format!("Starting on {}.", host.label));
            }
        }
        None => {
            *auto.write() = Auto { draft: None, phase: Phase::Unavailable };
            notice::push(state, Level::Warning, "Auto balance unavailable", "No machine reported enough free CPU and memory. Choose one.");
        }
    }
}
