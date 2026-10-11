//! "Publish repository": the three-step dialog behind the git control's primary action for a
//! repository without a remote (provider, repository, summary). Port of `PublishRepositoryDialog`
//! in `GitActionsControl.tsx`; the rules are in `t3_client::publish_repository`.
//!
//! The dialog reads the hosts the server found when it opens, publishes through
//! `sourceControl.publishRepository`, and shows the failure inside the dialog where the original
//! does. Nothing repaints while it is idle.
use crate::{
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt, View},
};
use dioxus::prelude::*;
use serde_json::Value;
use t3_client::{publish_repository as logic, source_control};
use t3_contracts::{SourceControlDiscoveryResult, SourceControlProviderKind};

#[derive(Clone)]
enum Scan {
    Loading,
    Failed(String),
    Ready(SourceControlDiscoveryResult),
}

#[component]
pub(crate) fn Dialog(state: Store<UiModel>, transport: TransportHandle, cwd: String, on_close: EventHandler<()>) -> Element {
    let mut scan = use_signal(|| Scan::Loading);
    let mut step = use_signal(|| 0usize);
    let mut chosen = use_signal(|| None::<SourceControlProviderKind>);
    let mut repository = use_signal(|| None::<String>);
    let mut private = use_signal(|| true);
    let mut remote = use_signal(|| "origin".to_owned());
    let mut https = use_signal(|| false);
    let mut advanced = use_signal(|| false);
    let mut failure = use_signal(|| None::<String>);
    let mut pending = use_signal(|| false);
    let mut result = use_signal(|| None::<Value>);

    use_resource({
        let transport = transport.clone();
        move || {
            let transport = transport.clone();
            async move {
                let owner = runtime::response_owner(&transport, state);
                let read = runtime::request_value_reported(transport.clone(), state, source_control::DISCOVER, serde_json::json!({})).await.and_then(|value| source_control::decode_discovery(&value));
                if owner.is_none() || owner != runtime::response_owner(&transport, state) {
                    return;
                }
                scan.set(match read {
                    Ok(found) => Scan::Ready(found),
                    Err(message) => Scan::Failed(message),
                });
            }
        }
    });

    let found = match &*scan.read() {
        Scan::Ready(found) => found.source_control_providers.clone(),
        _ => Vec::new(),
    };
    let scanned = matches!(&*scan.read(), Scan::Ready(_));
    let current = logic::current(chosen(), &found);
    let readiness = logic::readiness(current, &found);
    let host = logic::host_of(current);
    let shown_host = logic::host(current, &found);
    let text = repository().unwrap_or_else(|| logic::prefill(current, &found));
    let can_submit = readiness.ready && !pending() && logic::repository_complete(&text);
    let any_ready = logic::OPTIONS.iter().any(|option| logic::readiness(option.kind, &found).ready);
    let finished = result.read().clone();

    // Read from the signals at the moment of the click, so Enter and the button agree.
    let submit = use_callback({
        let (transport, cwd) = (transport.clone(), cwd.clone());
        move |()| {
            let found = match &*scan.peek() {
                Scan::Ready(found) => found.source_control_providers.clone(),
                _ => Vec::new(),
            };
            let current = logic::current(*chosen.peek(), &found);
            let text = repository.peek().clone().unwrap_or_else(|| logic::prefill(current, &found));
            if !logic::readiness(current, &found).ready || *pending.peek() || !logic::repository_complete(&text) {
                return;
            }
            failure.set(None);
            pending.set(true);
            let payload = logic::request(&cwd, current, &text, *private.peek(), &remote.peek(), *https.peek());
            let transport = transport.clone();
            spawn(async move {
                let owner = runtime::response_owner(&transport, state);
                let answer = runtime::request_value_reported(transport.clone(), state, logic::PUBLISH, payload).await;
                if owner.is_none() || owner != runtime::response_owner(&transport, state) {
                    return;
                }
                pending.set(false);
                match answer {
                    Ok(value) => {
                        result.set(Some(value));
                        step.set(2);
                    }
                    Err(message) => failure.set(Some(message)),
                }
            });
        }
    });
    let close = move |_| on_close.call(());

    rsx! {
        div { class: "modal-backdrop",
            div { class: "dialog publish-dialog", role: "dialog", "aria-modal": "true", "aria-labelledby": "publish-heading", "aria-describedby": "publish-description", tabindex: "-1",
                onmounted: move |event| { let element = event.data(); spawn(async move { let _ = element.set_focus(true).await; }); },
                onkeydown: move |event| { if event.key() == Key::Escape && !pending() { event.stop_propagation(); on_close.call(()); } },
                h2 { id: "publish-heading", "Publish repository" }
                p { id: "publish-description", "Pick where to host it, then point us at a repo to push to." }
                ol { class: "publish-steps", "aria-label": "Steps",
                    for (index, name) in logic::STEPS.iter().enumerate() {
                        li { key: "{name}", "aria-current": if step() == index { "step" } else { "false" }, class: if step() == index { "publish-step current" } else { "publish-step" },
                            "{name}"
                            if index == 0 { span { class: "muted", " \u{b7} {logic::host_of(current).label}" } }
                        }
                    }
                }
                if step() == 0 {
                    div { class: "publish-panel",
                        span { id: "publish-provider-label", class: "publish-label", "Provider" }
                        if let Scan::Failed(message) = &*scan.read() { p { role: "alert", class: "publish-error", "Could not read your source control providers: {message}" } }
                        div { class: "publish-cards", role: "radiogroup", "aria-labelledby": "publish-provider-label",
                            for option in logic::sorted(&found) {
                                {
                                    let ready = logic::readiness(option.kind, &found);
                                    let selected = current == option.kind && ready.ready;
                                    let kind = option.kind;
                                    rsx! {
                                        if ready.ready {
                                            button { key: "{option.label}", class: if selected { "publish-card selected" } else { "publish-card" }, r#type: "button", role: "radio", "aria-checked": "{selected}", "aria-label": "{option.label}",
                                                onclick: move |_| { chosen.set(Some(kind)); repository.set(None); },
                                                "{option.label}" }
                                        } else {
                                            div { key: "{option.label}", class: "publish-card unavailable", role: "radio", "aria-checked": "false", "aria-disabled": "true", "aria-label": "{option.label}",
                                                span { "{option.label}" }
                                                button { class: "publish-setup", r#type: "button", title: "{ready.hint.clone().unwrap_or_else(|| \"Open Settings -> Source Control to configure this provider.\".into())}",
                                                    onclick: move |_| { state.view().set(View::SourceControl); state.sidebar_open().set(false); on_close.call(()); }, "Setup Required" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        if scanned && !any_ready { p { class: "publish-hint", "No provider is ready. Open Settings -> Source Control for setup guidance." } }
                    }
                } else if step() == 1 {
                    div { class: "publish-panel",
                        label { r#for: "publish-repository-path", class: "publish-label", "Repository" }
                        div { class: "publish-path",
                            span { class: "publish-host", "{shown_host}/" }
                            input { id: "publish-repository-path", name: "publish-repository-path", "aria-label": "Repository", value: "{text}", placeholder: "{host.path_placeholder}", disabled: pending(),
                                oninput: move |event| repository.set(Some(event.value())),
                                onkeydown: move |event| { if event.key() == Key::Enter { event.prevent_default(); submit.call(()); } } }
                        }
                        span { id: "publish-visibility-label", class: "publish-label", "Visibility" }
                        div { class: "publish-cards", role: "radiogroup", "aria-labelledby": "publish-visibility-label",
                            button { class: if private() { "publish-card selected" } else { "publish-card" }, r#type: "button", role: "radio", "aria-checked": "{private()}", "aria-label": "Private", disabled: pending(), onclick: move |_| private.set(true),
                                span { "Private" } span { class: "muted", "Only invited people" } }
                            button { class: if private() { "publish-card" } else { "publish-card selected" }, r#type: "button", role: "radio", "aria-checked": "{!private()}", "aria-label": "Public", disabled: pending(), onclick: move |_| private.set(false),
                                span { "Public" } span { class: "muted", "Anyone on the web" } }
                        }
                        button { class: "publish-advanced", r#type: "button", "aria-expanded": "{advanced()}", onclick: move |_| advanced.set(!advanced()), if advanced() { "\u{25be} Advanced" } else { "\u{25b8} Advanced" } }
                        if advanced() {
                            div { class: "publish-options",
                                label { r#for: "publish-remote-name", "Remote" }
                                input { id: "publish-remote-name", "aria-label": "Remote", value: "{remote}", placeholder: "origin", disabled: pending(), oninput: move |event| remote.set(event.value()) }
                                span { id: "publish-protocol-label", class: "publish-label", "Protocol" }
                                div { class: "publish-cards", role: "radiogroup", "aria-labelledby": "publish-protocol-label",
                                    button { class: if https() { "publish-card" } else { "publish-card selected" }, r#type: "button", role: "radio", "aria-checked": "{!https()}", "aria-label": "SSH", disabled: pending(), onclick: move |_| https.set(false), "SSH" }
                                    button { class: if https() { "publish-card selected" } else { "publish-card" }, r#type: "button", role: "radio", "aria-checked": "{https()}", "aria-label": "HTTPS", disabled: pending(), onclick: move |_| https.set(true), "HTTPS" }
                                }
                            }
                        }
                        if pending() { p { role: "status", "aria-live": "polite", class: "publish-hint", "Publishing repository to {host.label}..." } }
                        if let (Some(message), false) = (failure(), pending()) {
                            div { role: "alert", class: "publish-error", p { class: "publish-error-title", "Publish failed" } crate::error_text::ErrorText { text: message } }
                        }
                    }
                } else if let Some(done) = finished {
                    {
                        let (title, detail) = logic::outcome(&done, host.label);
                        let name = done["repository"]["nameWithOwner"].as_str().unwrap_or("").to_owned();
                        let link = done["repository"]["url"].as_str().unwrap_or("").to_owned();
                        rsx! {
                            div { class: "publish-panel publish-done",
                                h3 { "{title}" }
                                p { class: "muted", "{detail}" }
                                p { class: "publish-name", "{name}" }
                                if link.starts_with("https://") || link.starts_with("http://") || link.starts_with("file://") {
                                    a { class: "publish-open", href: "{link}", target: "_blank", rel: "noopener noreferrer", "Open on {host.label}" }
                                }
                            }
                        }
                    }
                } else {
                    div { class: "publish-panel", p { class: "muted", "Publish result unavailable." } }
                }
                div { class: "dialog-actions",
                    if step() == 2 {
                        button { class: "primary", r#type: "button", "aria-label": "Done", onclick: close, "Done" }
                    } else {
                        button { r#type: "button", disabled: pending(), "aria-label": if step() == 0 { "Cancel" } else { "Back" },
                            onclick: move |_| if step() == 0 { on_close.call(()) } else { step.set(step().saturating_sub(1)) },
                            if step() == 0 { "Cancel" } else { "Back" } }
                        if step() == 0 {
                            button { class: "primary", r#type: "button", "aria-label": "Next", disabled: !any_ready || !readiness.ready, onclick: move |_| step.set(1), "Next" }
                        } else {
                            button { class: "primary", r#type: "button", "aria-label": "Publish", disabled: !can_submit, onclick: move |_| submit.call(()), if pending() { "Publishing..." } else { "Publish" } }
                        }
                    }
                }
            }
        }
    }
}
