//! A pull request's place in a stack and the popover that lists the stack's layers
//! (`PullRequestStackPopover.tsx`, `PullRequestStackLayers.tsx`). The layers are read when the
//! popover opens, so a list of rows does not fetch a stack for each; choosing a layer opens its
//! pull request in the panel beside the thread.
//!
//! In the pull request's own panel the stack is read with the pull request, and `Menu` (`PullRequestStackMenu.tsx`) adds what
//! can be done to the whole stack: merge it up to the layer in view, or rebase it. Both ask first, name the heads that were
//! read so a stack that moved is refused, and read the stack again whatever the answer (earlier layers of a rebase that failed
//! stay pushed). The rules and the words are `t3_client::pull_request_stack`.
use crate::{
    notice::{self, Level},
    pull_request_panel::{self, Target},
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
};
use dioxus::prelude::*;
use serde_json::Value;
use std::rc::Rc;
use t3_client::{
    pull_request::{self as pr, Reference},
    pull_request_stack::{self as logic, Action},
    thread_pull_requests::State,
};

pub(crate) const STACK: &str = "pullRequests.stack";

/// The words of the chip: layer `position` of `size`.
pub(crate) fn chip_label(number: u64, position: u64, size: u64) -> String {
    format!("Stack {number}, layer {position} of {size}")
}

/// What the panel holds of its stack: the last one read, kept through a failed refresh, and how the latest read went.
#[derive(Clone, PartialEq, Default)]
pub(crate) struct Held {
    pub data: Option<Rc<Value>>,
    /// The latest read succeeded (a pull request that is in no stack is a read that succeeded with nothing).
    pub succeeded: bool,
    pub pending: bool,
    pub error: Option<String>,
}

/// The stack the open environment's threads saved for this pull request (`savedPullRequestStack`), so the layers can still be
/// navigated while the host's own read is loading or has failed. Subscribes the caller to the shell; the links read are only the
/// ones of this pull request's repository, so a busy shell costs a scan and no serialization.
pub(crate) fn saved(state: Store<UiModel>, reference: &Reference) -> Option<Rc<Value>> {
    let (host, repository) = (reference.host.as_deref()?, reference.repository.as_str());
    let shell = state.shell();
    let shell = shell.read();
    let links: Vec<Value> = shell
        .snapshot
        .as_ref()?
        .threads
        .iter()
        .flat_map(|thread| thread.pull_requests.iter().flatten().flatten())
        .filter(|link| link.host.as_str().eq_ignore_ascii_case(host) && link.repository.as_str().eq_ignore_ascii_case(repository))
        .filter_map(|link| serde_json::to_value(link).ok())
        .collect();
    t3_client::thread_pull_requests::saved_stack(&links, reference).map(Rc::new)
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
    let saved = use_memo({
        let reference = reference.clone();
        move || saved(state, &reference)
    });
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
            // Placed under the chip in the viewport, inside it: a list or a panel that clips its children would cut the popover off.
            onmounted: move |event| {
                drop(event.set_focus(true));
                dioxus::document::eval("const m=document.querySelector('.pr-stack-menu');const c=m?.parentElement?.querySelector('.pr-stack-chip');if(m&&c){const r=c.getBoundingClientRect();const w=m.offsetWidth,h=m.offsetHeight;m.style.position='fixed';m.style.left=Math.max(8,Math.min(r.left,window.innerWidth-w-8))+'px';m.style.top=(r.bottom+h+8>window.innerHeight?Math.max(8,r.top-h-4):r.bottom+4)+'px';}");
            },
            onclick: move |event| event.stop_propagation(),
            onkeydown: move |event| { if event.key() == Key::Escape { event.stop_propagation(); on_close.call(()); } },
            p { class: "pr-stack-title", "Stack #{number}" }
            // While the host's stack is being read, or could not be, the layers the threads saved keep it navigable.
            match (&*read.read(), saved()) {
                (Read::Loading, Some(stack)) => rsx! {
                    p { class: "muted", role: "status", "{logic::view(true, false, true, None).notice.unwrap_or_default()}" }
                    LayerList { state, thread_id: thread_id.clone(), reference: reference.clone(), stack, disabled: false, on_close }
                },
                (Read::Failed(message), Some(stack)) => rsx! {
                    p { class: "muted", role: "status", "{logic::view(true, false, false, Some(message)).notice.unwrap_or_default()}" }
                    button { class: "menu-item", role: "menuitem", r#type: "button", onclick: move |_| attempt.set(attempt() + 1), "Retry stack refresh" }
                    LayerList { state, thread_id: thread_id.clone(), reference: reference.clone(), stack, disabled: false, on_close }
                },
                (Read::Loading, None) => rsx! { p { class: "muted", role: "status", "Loading stack\u{2026}" } },
                (Read::Gone, _) => rsx! { p { class: "muted", role: "status", "This pull request is no longer in a stack." } },
                (Read::Failed(message), None) => rsx! {
                    p { class: "error-banner", role: "alert", "{message}" }
                    button { class: "menu-item", role: "menuitem", r#type: "button", onclick: move |_| attempt.set(attempt() + 1), "Retry stack refresh" }
                },
                (Read::Ready(stack), _) => rsx! { LayerList { state, thread_id: thread_id.clone(), reference: reference.clone(), stack: stack.clone(), disabled: false, on_close } },
            }
        }
    }
}

/// The layers of a stack, top first, each opening its pull request in the panel; the base under them.
#[component]
fn LayerList(state: Store<UiModel>, thread_id: String, reference: Reference, stack: Rc<Value>, disabled: bool, on_close: EventHandler<()>) -> Element {
    rsx! {
        div { class: "pr-stack-layers",
            for layer in stack["layers"].as_array().into_iter().flatten().rev() {
                {
                    let layer_number = layer["number"].as_u64().unwrap_or(0);
                    let current = layer_number == reference.number;
                    let layer_state = State::of(layer["state"].as_str().unwrap_or("open"), layer["isDraft"].as_bool().unwrap_or(false));
                    let title = layer["title"].as_str().filter(|title| !title.is_empty()).or_else(|| layer["headBranch"].as_str()).unwrap_or_default().to_owned();
                    let branch = layer["headBranch"].as_str().unwrap_or_default().to_owned();
                    let target = Target { thread_id: thread_id.clone(), reference: Reference { number: layer_number, ..reference.clone() } };
                    rsx! { button { key: "{layer_number}", class: "menu-item pr-stack-layer", role: "menuitem", r#type: "button", "aria-label": "{title}, #{layer_number}", "aria-current": if current { "true" } else { "false" }, disabled,
                        onclick: move |_| { on_close.call(()); pull_request_panel::open(state, target.clone()); },
                        crate::pull_request_glyph::Glyph { state: layer_state, stack: false }
                        span { class: "pr-stack-layer-text", span { class: "pr-stack-layer-title", "{title}" } span { class: "muted", "#{layer_number} \u{b7} {branch} \u{b7} {layer_state.label()}" } }
                        if current { span { "aria-hidden": "true", "\u{2713}" } }
                    } }
                }
            }
            p { class: "muted pr-stack-base", "\u{21b3} {stack[\"base\"].as_str().unwrap_or_default()}" }
        }
    }
}

/// The stack of the pull request in view, read with it: a chip that opens the layers and what can be done to the stack, the
/// button that merges it, and the questions asked before either action. `notice` says when what is shown may be old, and `retry`
/// reads it again; `can_merge` and `can_rebase` are the reader's rights over a stack that was read just now.
#[component]
pub(crate) fn Menu(state: Store<UiModel>, transport: TransportHandle, thread_id: String, reference: Reference, stack: Rc<Value>, notice: Option<String>, retry: Option<Callback<()>>, can_merge: bool, can_rebase: bool, merge_method: String, on_acted: Callback<()>) -> Element {
    let mut open = use_signal(|| false);
    let mut confirming = use_signal(|| None::<Action>);
    let mut pending = use_signal(|| false);
    let Some(parsed) = logic::Stack::parse(&stack) else { return rsx! {} };
    let plan = logic::plan(&parsed, reference.number, pending());
    let size = parsed.layers.len();
    let label = chip_label(parsed.number, plan.position as u64, size as u64);
    let tooltip = format!("View stack #{}, layer {} of {size}{}", parsed.number, plan.position, notice.as_ref().map(|notice| format!(" \u{b7} {notice}")).unwrap_or_default());
    let selected_open = plan.selected.as_ref().is_some_and(|layer| layer.state == "open");
    let run = {
        let (transport, reference, parsed, merge_method) = (transport.clone(), reference.clone(), parsed.clone(), merge_method.clone());
        move |action: Action| {
            if *pending.peek() {
                return;
            }
            let plan = logic::plan(&parsed, reference.number, false);
            let allowed = match action {
                Action::Merge => can_merge,
                Action::Rebase => can_rebase,
            };
            let Some(input) = allowed.then(|| logic::run_input(&parsed, &plan, &reference, action, &merge_method)).flatten() else { return };
            pending.set(true);
            let transport = transport.clone();
            spawn(async move {
                let owner = runtime::response_owner(&transport, state);
                let answer = runtime::request_value_reported(transport.clone(), state, pr::RUN_ACTION, input).await;
                if owner.is_none() || owner != runtime::response_owner(&transport, state) {
                    pending.set(false);
                    return;
                }
                pending.set(false);
                confirming.set(None);
                // Whatever the answer, the stack is read again: earlier layers of a rebase that stopped stay pushed.
                on_acted.call(());
                match answer {
                    Ok(_) => {
                        let (title, detail) = logic::success(action);
                        notice::push(state, Level::Success, title, detail.unwrap_or_default());
                    }
                    Err(message) => notice::push(state, Level::Error, logic::FAILURE_TITLE, logic::failure_text(&message)),
                }
            });
        }
    };
    rsx! {
        span { class: "pr-stack-chip-wrap",
            button { class: "pr-stack-chip", r#type: "button", "aria-label": "{label}", "aria-haspopup": "menu", "aria-expanded": "{open()}",
                title: "{tooltip}",
                onclick: move |event| { event.stop_propagation(); open.set(!open()); },
                crate::pull_request_glyph::Glyph { state: State::Open, stack: true } "{plan.position}/{size}"
                if retry.is_some() { span { class: "pr-stack-warning", "aria-hidden": "true", "\u{26a0}" } } }
            if open() {
                div { class: "menu-backdrop", onclick: move |event| { event.stop_propagation(); open.set(false); } }
                div { class: "context-menu pr-stack-menu", role: "menu", "aria-label": "Stack #{parsed.number}", tabindex: "0",
                    onmounted: move |event| {
                        drop(event.set_focus(true));
                        dioxus::document::eval("const m=document.querySelector('.pr-stack-menu');const c=m?.parentElement?.querySelector('.pr-stack-chip');if(m&&c){const r=c.getBoundingClientRect();const w=m.offsetWidth,h=m.offsetHeight;m.style.position='fixed';m.style.left=Math.max(8,Math.min(r.left,window.innerWidth-w-8))+'px';m.style.top=(r.bottom+h+8>window.innerHeight?Math.max(8,r.top-h-4):r.bottom+4)+'px';}");
                    },
                    onclick: move |event| event.stop_propagation(),
                    onkeydown: move |event| { if event.key() == Key::Escape { event.stop_propagation(); open.set(false); } },
                    p { class: "pr-stack-title", "Stack #{parsed.number}"
                        if notice.is_some() { span { role: "status", class: "muted", title: "{notice.clone().unwrap_or_default()}", if retry.is_some() { " May be stale" } else { " Refreshing\u{2026}" } } } }
                    if let Some(retry) = retry {
                        button { class: "menu-item", role: "menuitem", r#type: "button", onclick: move |_| { open.set(false); retry.call(()); }, "Retry stack refresh" }
                    }
                    LayerList { state, thread_id: thread_id.clone(), reference: reference.clone(), stack: stack.clone(), disabled: pending(), on_close: move |_| open.set(false) }
                    if can_merge || can_rebase {
                        hr { class: "menu-separator" }
                        if can_merge {
                            button { class: "menu-item", role: "menuitem", r#type: "button", "aria-label": "Merge stack ({plan.merge_layers.len()})", disabled: plan.merge_disabled, onclick: move |_| { open.set(false); confirming.set(Some(Action::Merge)); }, "Merge stack ({plan.merge_layers.len()})" }
                        }
                        if can_rebase {
                            button { class: "menu-item", role: "menuitem", r#type: "button", "aria-label": "Rebase stack", disabled: plan.rebase_disabled, onclick: move |_| { open.set(false); confirming.set(Some(Action::Rebase)); }, "Rebase stack" }
                        }
                        if plan.merge_blocked { p { class: "muted pr-stack-base", "Every layer being merged must be open and ready for review." } }
                    }
                }
            }
        }
        if can_merge && selected_open {
            button { class: "primary pr-stack-merge", r#type: "button", "aria-label": "Merge stack", title: "{logic::merge_tooltip(reference.number, &parsed.base, plan.merge_layers.len())}", disabled: plan.merge_disabled, onclick: move |_| confirming.set(Some(Action::Merge)), "Merge stack" }
        }
        if let Some(action) = confirming() {
            {
                let layers = plan.layers(action).to_vec();
                let (title, description) = logic::confirmation(action, reference.number, &parsed.base, &merge_method, layers.len());
                let mut run = run.clone();
                rsx! {
                    div { class: "modal-backdrop",
                        div { class: "dialog pr-confirm", role: "alertdialog", "aria-modal": "true", "aria-labelledby": "pr-stack-confirm-heading", "aria-describedby": "pr-stack-confirm-description", tabindex: "-1",
                            onmounted: move |event| { let element = event.data(); spawn(async move { let _ = element.set_focus(true).await; }); },
                            onkeydown: move |event| { if event.key() == Key::Escape && !*pending.peek() { event.stop_propagation(); confirming.set(None); } },
                            h2 { id: "pr-stack-confirm-heading", "{title}" }
                            p { id: "pr-stack-confirm-description", "{description}" }
                            ul { class: "pr-stack-confirm-layers",
                                for layer in layers.iter() {
                                    li { key: "{layer.number}", class: "pr-stack-confirm-layer",
                                        crate::pull_request_glyph::Glyph { state: State::of(&layer.state, layer.is_draft), stack: false }
                                        span { class: "pr-stack-layer-text", span { class: "pr-stack-layer-title", "{logic::Stack::label(layer)}" } span { class: "muted", "#{layer.number} \u{b7} {State::of(&layer.state, layer.is_draft).label()}" } }
                                    }
                                }
                            }
                            div { class: "dialog-actions",
                                button { r#type: "button", "aria-label": "Cancel", disabled: pending(), onclick: move |_| confirming.set(None), "Cancel" }
                                button { class: "primary", r#type: "button", "aria-label": "Confirm {logic::confirm_label(action)}", disabled: pending() || !match action { Action::Merge => can_merge, Action::Rebase => can_rebase },
                                    onclick: move |_| run(action),
                                    if pending() { "Working\u{2026}" } else { "{logic::confirm_label(action)}" } }
                            }
                        }
                    }
                }
            }
        }
    }
}
