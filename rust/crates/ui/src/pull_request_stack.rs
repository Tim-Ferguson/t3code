//! A pull request's place in a stack and the popover that lists the stack's layers
//! (`PullRequestStackPopover.tsx`, `PullRequestStackLayers.tsx`). The layers are read when the
//! popover opens, so a list of rows does not fetch a stack for each; choosing a layer opens its
//! pull request in the panel beside the thread.
use crate::{
    pull_request_panel::{self, Target},
    runtime::{self, TransportHandle, UiModel},
};
use dioxus::prelude::*;
use serde_json::Value;
use std::rc::Rc;
use t3_client::{pull_request::Reference, thread_pull_requests::State};

pub(crate) const STACK: &str = "pullRequests.stack";

/// The words of the chip: layer `position` of `size`.
pub(crate) fn chip_label(number: u64, position: u64, size: u64) -> String {
    format!("Stack {number}, layer {position} of {size}")
}

#[derive(Default)]
enum Read {
    #[default]
    Loading,
    Ready(Rc<Value>),
    Gone,
    Failed(String),
}

/// "position/size" beside a pull request, opening the stack it belongs to.
#[component]
pub(crate) fn Chip(state: Store<UiModel>, transport: TransportHandle, thread_id: String, reference: Reference, number: u64, position: u64, size: u64) -> Element {
    let mut open = use_signal(|| false);
    let label = chip_label(number, position, size);
    rsx! {
        span { class: "pr-stack-chip-wrap",
            button { class: "pr-stack-chip", r#type: "button", "aria-label": "{label}", "aria-haspopup": "menu", "aria-expanded": "{open()}", title: "View stack #{number}, layer {position} of {size}",
                onclick: move |event| { event.stop_propagation(); open.set(!open()); },
                crate::pull_request_glyph::Glyph { state: State::Open, stack: true } "{position}/{size}" }
            if open() {
                div { class: "menu-backdrop", onclick: move |event| { event.stop_propagation(); open.set(false); } }
                Layers { state, transport, thread_id, reference, number, on_close: move |_| open.set(false) }
            }
        }
    }
}

/// The popover: mounted only while open, which is when the stack is read.
#[component]
fn Layers(state: Store<UiModel>, transport: TransportHandle, thread_id: String, reference: Reference, number: u64, on_close: EventHandler<()>) -> Element {
    let mut read = use_signal(Read::default);
    let mut attempt = use_signal(|| 0u32);
    use_resource({
        let (transport, reference) = (transport.clone(), reference.clone());
        move || {
            let _ = attempt();
            let (transport, reference) = (transport.clone(), reference.clone());
            async move {
                read.set(Read::Loading);
                let owner = runtime::response_owner(&transport, state);
                let answer = runtime::request_value_reported(transport.clone(), state, STACK, reference.payload(serde_json::json!({}))).await;
                if owner.is_none() || owner != runtime::response_owner(&transport, state) {
                    return;
                }
                read.set(match answer {
                    Ok(Value::Null) => Read::Gone,
                    Ok(value) => Read::Ready(Rc::new(value)),
                    Err(message) => Read::Failed(message),
                });
            }
        }
    });
    rsx! {
        div { class: "context-menu pr-stack-menu", role: "menu", "aria-label": "Stack #{number}", tabindex: "0",
            onmounted: move |event| { drop(event.set_focus(true)); },
            onclick: move |event| event.stop_propagation(),
            onkeydown: move |event| { if event.key() == Key::Escape { event.stop_propagation(); on_close.call(()); } },
            p { class: "pr-stack-title", "Stack #{number}" }
            match &*read.read() {
                Read::Loading => rsx! { p { class: "muted", role: "status", "Loading stack\u{2026}" } },
                Read::Gone => rsx! { p { class: "muted", role: "status", "This pull request is no longer in a stack." } },
                Read::Failed(message) => rsx! {
                    p { class: "error-banner", role: "alert", "{message}" }
                    button { class: "menu-item", role: "menuitem", r#type: "button", onclick: move |_| attempt.set(attempt() + 1), "Retry stack refresh" }
                },
                Read::Ready(stack) => rsx! {
                    div { class: "pr-stack-layers",
                        for layer in stack["layers"].as_array().into_iter().flatten().rev() {
                            {
                                let layer_number = layer["number"].as_u64().unwrap_or(0);
                                let current = layer_number == reference.number;
                                let layer_state = State::of(layer["state"].as_str().unwrap_or("open"), layer["isDraft"].as_bool().unwrap_or(false));
                                let title = layer["title"].as_str().filter(|title| !title.is_empty()).or_else(|| layer["headBranch"].as_str()).unwrap_or_default().to_owned();
                                let branch = layer["headBranch"].as_str().unwrap_or_default().to_owned();
                                let target = Target { thread_id: thread_id.clone(), reference: Reference { number: layer_number, ..reference.clone() } };
                                rsx! { button { key: "{layer_number}", class: "menu-item pr-stack-layer", role: "menuitem", r#type: "button", "aria-label": "{title}, #{layer_number}", "aria-current": if current { "true" } else { "false" },
                                    onclick: move |_| { on_close.call(()); pull_request_panel::open(state, target.clone()); },
                                    crate::pull_request_glyph::Glyph { state: layer_state, stack: false }
                                    span { class: "pr-stack-layer-text", span { class: "pr-stack-layer-title", "{title}" } span { class: "muted", "#{layer_number} \u{b7} {branch} \u{b7} {layer_state.label()}" } }
                                    if current { span { "aria-hidden": "true", "\u{2713}" } }
                                } }
                            }
                        }
                        p { class: "muted pr-stack-base", "\u{21b3} {stack[\"base\"].as_str().unwrap_or_default()}" }
                    }
                },
            }
        }
    }
}
