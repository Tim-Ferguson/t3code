//! Starting a thread from a pull request: name it (a URL, a checkout command or a number), have the
//! host resolve it, then prepare it in the project's own checkout or in a worktree of its own, and
//! begin a new thread there. Port of `PullRequestThreadDialog.tsx`.
//!
//! The original resolves while the reference is typed, after a pause. Nothing here waits on a
//! timer: Enter, or the button, resolves what is typed, and Enter on a resolved reference prepares a
//! worktree, as the original's Enter does.
use crate::{
    notice::{self, Level},
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
    settings_common::connected_allows,
};
use dioxus::prelude::*;
use serde_json::{Value, json};
use t3_client::pull_request_reference as reference_of;
use t3_contracts::AuthEnvironmentScope;

/// The project a pull request thread starts in, and what the box starts with.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Open {
    pub(crate) project_id: String,
    pub(crate) cwd: String,
    pub(crate) reference: String,
}

pub(crate) fn open(state: Store<UiModel>, project_id: &str, cwd: &str, reference: &str) {
    state.pr_checkout().set(Some(Open { project_id: project_id.to_owned(), cwd: cwd.to_owned(), reference: reference.to_owned() }));
}
pub(crate) fn close(state: Store<UiModel>) {
    state.pr_checkout().set(None);
}

#[component]
pub(crate) fn Host(state: Store<UiModel>, transport: TransportHandle) -> Element {
    match state.pr_checkout().read().clone() {
        Some(request) => rsx! { Dialog { state, transport, request } },
        None => rsx! {},
    }
}

#[component]
fn Dialog(state: Store<UiModel>, transport: TransportHandle, request: Open) -> Element {
    let mut reference = use_signal(|| request.reference.clone());
    let mut dirty = use_signal(|| false);
    // What the host said of the reference it was last asked about.
    let mut resolved = use_signal(|| None::<(String, Value)>);
    let mut resolving = use_signal(|| false);
    let mut preparing = use_signal(|| None::<&'static str>);
    let mut failure = use_signal(|| None::<String>);
    let may_write = connected_allows(state, AuthEnvironmentScope::SourceControlWrite);
    let may_operate = connected_allows(state, AuthEnvironmentScope::OrchestrationOperate);
    let parsed = reference_of::parse(&reference.read());
    let validation = if !*dirty.read() { None } else if reference.read().trim().is_empty() { Some(reference_of::EMPTY) } else if parsed.is_none() { Some(reference_of::INVALID) } else { None };
    let found = resolved.read().clone().filter(|(asked, _)| Some(asked) == parsed.as_ref()).map(|(_, found)| found);
    let cwd = request.cwd.clone();
    let resolve = {
        let (transport, cwd) = (transport.clone(), cwd.clone());
        use_callback(move |_: ()| {
            dirty.set(true);
            let Some(asked) = reference_of::parse(&reference.peek()) else { return };
            if *resolving.peek() || preparing.peek().is_some() {
                return;
            }
            resolving.set(true);
            failure.set(None);
            let (transport, cwd) = (transport.clone(), cwd.clone());
            spawn(async move {
                let owner = runtime::response_owner(&transport, state);
                let answer = runtime::request_value_reported(transport.clone(), state, "git.resolvePullRequest", json!({"cwd":cwd,"reference":asked})).await;
                if owner.is_some() && owner == runtime::response_owner(&transport, state) {
                    match answer {
                        Ok(value) => resolved.set(Some((asked, value["pullRequest"].clone()))),
                        Err(message) => failure.set(Some(message)),
                    }
                }
                resolving.set(false);
            });
        })
    };
    let prepare = {
        let (transport, request) = (transport.clone(), request.clone());
        use_callback(move |mode: &'static str| {
            let Some(asked) = reference_of::parse(&reference.peek()) else {
                dirty.set(true);
                return;
            };
            let allowed = may_write && (mode == "local" || may_operate);
            if !allowed || preparing.peek().is_some() || *resolving.peek() || resolved.peek().as_ref().is_none_or(|(known, _)| *known != asked) {
                return;
            }
            preparing.set(Some(mode));
            failure.set(None);
            let mut input = json!({"cwd":request.cwd,"reference":asked,"mode":mode});
            if mode == "worktree" {
                input["threadId"] = json!(format!("thread-{}", uuid::Uuid::new_v4()));
            }
            let (transport, request) = (transport.clone(), request.clone());
            spawn(async move {
                let owner = runtime::response_owner(&transport, state);
                let answer = runtime::request_value_reported(transport.clone(), state, "git.preparePullRequestThread", input).await;
                if owner.is_some() && owner == runtime::response_owner(&transport, state) {
                    match answer {
                        Ok(done) => {
                            let branch = done["branch"].as_str().unwrap_or_default().to_owned();
                            runtime::new_thread(&transport, state, Some(request.project_id.clone()));
                            crate::new_thread::apply_prepared(state, &branch, done["worktreePath"].as_str().map(str::to_owned));
                            notice::push(state, Level::Success, format!("Pull request #{} is ready", done["pullRequest"]["number"].as_u64().unwrap_or(0)), if mode == "worktree" { "A worktree holds its branch." } else { "Its branch is checked out." });
                            close(state);
                        }
                        Err(message) => failure.set(Some(message)),
                    }
                }
                preparing.set(None);
            });
        })
    };
    let busy = preparing.read().is_some();
    let error = validation.map(str::to_owned).or_else(|| failure.read().clone());
    rsx! { div { class: "modal-backdrop", onclick: move |_| { if !busy { close(state) } },
        div { class: "dialog wide-dialog", role: "dialog", "aria-modal": "true", "aria-label": "Checkout pull request", tabindex: "-1",
            onclick: move |event| event.stop_propagation(),
            onkeydown: move |event| { if event.key() == Key::Escape { event.stop_propagation(); if preparing.peek().is_none() { close(state) } } },
            h2 { "Checkout pull request" }
            p { class: "muted", "Resolve a pull request, then create the draft thread in the main repo or in a dedicated worktree." }
            label { r#for: "pr-thread-reference", "Pull request" }
            input { id: "pr-thread-reference", "aria-label": "Pull request", placeholder: "PR URL, checkout command, or #42", value: "{reference}", disabled: busy,
                onmounted: move |event| { let element = event.data(); spawn(async move { let _ = element.set_focus(true).await; }); },
                oninput: move |event| { dirty.set(true); reference.set(event.value()); },
                onkeydown: move |event| { if event.key() == Key::Enter { event.prevent_default(); if resolved.peek().as_ref().is_some_and(|(known, _)| Some(known) == reference_of::parse(&reference.peek()).as_ref()) { prepare.call("worktree") } else { resolve.call(()) } } } }
            button { r#type: "button", "aria-label": "Resolve pull request", disabled: busy || *resolving.read() || parsed.is_none(), onclick: move |_| resolve.call(()), if *resolving.read() { "Resolving pull request..." } else { "Resolve" } }
            if let Some(found) = found { div { class: "pr-resolved",
                p { strong { { found["title"].as_str().unwrap_or_default().to_owned() } } }
                p { class: "muted", "#{found[\"number\"]} \u{b7} {found[\"headBranch\"].as_str().unwrap_or_default()} to {found[\"baseBranch\"].as_str().unwrap_or_default()}" }
                p { class: "muted", { found["state"].as_str().unwrap_or_default().to_owned() } }
            } }
            if let Some(message) = error { p { class: "error-banner", role: "alert", "{message}" } }
            if !may_write { p { class: "muted", "This connection cannot change source control." } }
            div { class: "dialog-actions",
                button { r#type: "button", "aria-label": "Cancel", disabled: busy, onclick: move |_| close(state), "Cancel" }
                button { r#type: "button", "aria-label": "Local", disabled: busy || !may_write || resolved.read().is_none() || parsed.is_none(), onclick: move |_| prepare.call("local"), if *preparing.read() == Some("local") { "Preparing local..." } else { "Local" } }
                button { class: "primary", r#type: "button", "aria-label": "Worktree", disabled: busy || !may_write || !may_operate || resolved.read().is_none() || parsed.is_none(), onclick: move |_| prepare.call("worktree"), if *preparing.read() == Some("worktree") { "Preparing worktree..." } else { "Worktree" } }
            }
        }
    } }
}
