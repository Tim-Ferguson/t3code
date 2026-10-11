//! Adding a provider from the ACP registry (`AcpRegistrySearchStep.tsx`): the step of the add dialog that searches the registry
//! and prepares the agent that is chosen, and the button that carries on a browser sign-in an agent asked for
//! (`acceptAcpRegistryUrlAuth`). Preparing is the server downloading or installing what the agent runs, so the button says
//! which while it works; a refused or failed preparation is said where it happened.
//! This build's fixture server has no registry feed, so a search there ends in the server's own answer; the step shows that
//! answer rather than an empty list.
use crate::{
    notice::{self, Level},
    runtime::{self, TransportHandle, UiModel},
    settings_common::connected_allows,
};
use dioxus::prelude::*;
use serde_json::Value;
use t3_client::acp_registry as logic;
use t3_contracts::AuthEnvironmentScope;

/// The search box, the registry's compatible agents and the way on: a prepared agent goes to `on_prepared`.
#[component]
pub(crate) fn Step(state: Store<UiModel>, transport: TransportHandle, instances: Value, on_prepared: EventHandler<Value>, on_manual: EventHandler<()>, on_local: EventHandler<()>) -> Element {
    let mut query = use_signal(String::new);
    // An empty query is the compact catalog of compatible agents, so the step is useful before anyone knows what to search for.
    let mut submitted = use_signal(String::new);
    let mut results = use_signal(|| None::<Result<Vec<Value>, String>>);
    let mut preparing = use_signal(|| None::<String>);
    let mut failure = use_signal(|| None::<String>);
    let mut generation = use_signal(|| 0u64);
    let can_manage = connected_allows(state, AuthEnvironmentScope::ProvidersManage);
    let readable = connected_allows(state, AuthEnvironmentScope::OrchestrationRead);
    {
        let transport = transport.clone();
        use_resource(move || {
            let (transport, text, readable) = (transport.clone(), submitted.read().clone(), readable);
            async move {
                if !readable {
                    return;
                }
                let owner = runtime::response_owner(&transport, state);
                let answer = runtime::request_value_reported(transport.clone(), state, logic::SEARCH, logic::search_input(&text)).await;
                if owner.is_none() || owner != runtime::response_owner(&transport, state) {
                    return;
                }
                results.set(Some(answer.map(|value| value["agents"].as_array().cloned().unwrap_or_default())));
            }
        });
    }
    let mut typed = move |text: String| {
        query.set(text.clone());
        failure.set(None);
        let next = *generation.peek() + 1;
        generation.set(next);
        // Asked once the typing stops, never on every key.
        spawn(async move {
            runtime::delay(logic::SEARCH_DEBOUNCE_MS).await;
            if *generation.peek() == next {
                submitted.set(text.trim().to_owned());
            }
        });
    };
    let busy = preparing.read().is_some();
    let prepare = {
        let transport = transport.clone();
        move |agent: Value| {
            if preparing.peek().is_some() || !connected_allows(state, AuthEnvironmentScope::ProvidersManage) {
                return;
            }
            let id = agent["id"].as_str().unwrap_or_default().to_owned();
            preparing.set(Some(id.clone()));
            failure.set(None);
            let transport = transport.clone();
            spawn(async move {
                let owner = runtime::response_owner(&transport, state);
                let answer = runtime::request_value_reported(transport.clone(), state, logic::PREPARE, serde_json::json!({"agentId": id})).await;
                if owner.is_none() || owner != runtime::response_owner(&transport, state) {
                    return;
                }
                preparing.set(None);
                match answer {
                    Ok(prepared) => on_prepared.call(logic::applied(&agent, &prepared)),
                    Err(message) => failure.set(Some(if message.trim().is_empty() { logic::PREPARE_FAILED.to_owned() } else { message })),
                }
            });
        }
    };
    let shown = results.read().clone();
    rsx! {
        section { class: "acp-registry-step", "aria-labelledby": "acp-registry-search-heading",
            h3 { id: "acp-registry-search-heading", class: "sr-only", "Choose an agent" }
            div { class: "acp-registry-search",
                input { r#type: "search", "aria-label": "Search ACP Registry", placeholder: "Search agents\u{2026}", value: "{query}", disabled: busy, oninput: move |event| typed(event.value()) }
                button { r#type: "button", "aria-label": "Enter manually", disabled: busy, onclick: move |_| on_manual.call(()), "Enter manually" }
                button { r#type: "button", "aria-label": "Use a local ACP command", disabled: busy, onclick: move |_| on_local.call(()), "Local ACP command" }
            }
            div { class: "sr-only", role: "status", {match &shown {
                None => "Searching the ACP Registry.".to_owned(),
                Some(Ok(agents)) => format!("{} compatible {} found.", agents.len(), if agents.len() == 1 { "agent" } else { "agents" }),
                Some(Err(_)) => String::new(),
            }} }
            if let Some(message) = failure.read().clone().or_else(|| shown.as_ref().and_then(|answer| answer.as_ref().err().cloned())) { p { class: "error-banner", role: "status", "{message}" } }
            match shown {
                None => rsx! { p { class: "muted", "Searching the registry..." } },
                Some(Ok(agents)) if agents.is_empty() => rsx! { div { class: "acp-registry-empty", p { strong { "No compatible agents found" } } p { class: "muted", "Try a broader search." } } },
                Some(Ok(agents)) => rsx! {
                    div { class: "acp-registry-results",
                        for agent in agents {
                            {
                                let id = agent["id"].as_str().unwrap_or_default().to_owned();
                                let name = agent["name"].as_str().unwrap_or_default().to_owned();
                                let added = logic::is_configured(&instances, &id);
                                let working = preparing.read().as_deref() == Some(id.as_str());
                                let label = logic::button_label(&agent, added, working);
                                let named = format!("{} {name}", if added { "Already added" } else if working { label } else { "Add" });
                                let about = agent["website"].as_str().filter(|url| !url.is_empty()).or_else(|| agent["repository"].as_str().filter(|url| !url.is_empty())).map(str::to_owned);
                                let mut prepare = prepare.clone();
                                rsx! { article { key: "{id}", class: "acp-registry-agent",
                                    div { class: "acp-registry-agent-text", h4 { "{name}" } if let Some(description) = agent["description"].as_str().filter(|text| !text.is_empty()) { p { class: "muted", "{description}" } } }
                                    if let Some(url) = about { a { href: "{url}", target: "_blank", rel: "noreferrer", "aria-label": "About {name}", title: "About {name}", "\u{2197}" } }
                                    button { r#type: "button", "aria-label": "{named}", "aria-busy": "{working}", disabled: !can_manage || added || busy, onclick: move |_| prepare(agent.clone()), "{label}" }
                                } }
                            }
                        }
                    }
                },
                Some(Err(_)) => rsx! {},
            }
        }
    }
}

/// "Continue authentication": the agent asked for a browser sign-in. Following the link tells the server the person went
/// (`acceptAcpRegistryUrlAuth`), and a request that has expired says so.
#[component]
pub(crate) fn UrlAuth(state: Store<UiModel>, transport: TransportHandle, instance_id: String, action: Value) -> Element {
    let allowed = connected_allows(state, AuthEnvironmentScope::ProvidersManage);
    let (message, url, elicitation) = (action["message"].as_str().unwrap_or_default().to_owned(), action["url"].as_str().unwrap_or_default().to_owned(), action["elicitationId"].as_str().unwrap_or_default().to_owned());
    if url.is_empty() || elicitation.is_empty() {
        return rsx! {};
    }
    rsx! { div { class: "acp-url-auth",
        p { "{message}" }
        code { "{url}" }
        a { href: "{url}", target: "_blank", rel: "noopener noreferrer", "aria-disabled": "{!allowed}",
            onclick: move |_| {
                if !allowed {
                    return;
                }
                let (transport, instance_id, elicitation) = (transport.clone(), instance_id.clone(), elicitation.clone());
                spawn(async move {
                    match runtime::request_value_reported(transport, state, logic::ACCEPT_URL_AUTH, serde_json::json!({"instanceId": instance_id, "elicitationId": elicitation})).await {
                        Ok(result) if result["accepted"] == false => notice::push(state, Level::Warning, "Authentication request expired", "Refresh the provider and start the authentication flow again."),
                        Ok(_) => {}
                        Err(message) => notice::push(state, Level::Error, "Could not continue authentication", if message.trim().is_empty() { "The authentication request expired.".to_owned() } else { message }),
                    }
                });
            },
            "Continue authentication" }
    } }
}
