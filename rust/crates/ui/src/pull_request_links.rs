//! Which pull requests a thread is filed under, in its header, and filing one under it or taking it
//! out from the pull request's own panel. Port of the thread links of `PullRequestThreadLinks.tsx`;
//! the thread's links are the ones its shell carries (`pullRequests`, and the legacy
//! `linkedPullRequest`), so a link made anywhere shows up here without this view asking for it.
use crate::{
    notice::{self, Level},
    pull_request_panel::{self, Target},
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
    settings_common::connected_allows,
};
use dioxus::prelude::*;
use serde_json::{Value, json};
use t3_client::{pull_request::Reference, thread_pull_requests as logic};
use t3_contracts::{AuthEnvironmentScope, ThreadShell};

/// The thread's pull requests as buttons that open the panel on one. The one the branch control's
/// badge already offers is left to the badge.
#[component]
pub(crate) fn HeaderLinks(state: Store<UiModel>, transport: TransportHandle, thread: ThreadShell) -> Element {
    let value = serde_json::to_value(&thread).unwrap_or(Value::Null);
    let badge = state.pr_available().read().clone();
    let project_id = thread.project_id.to_string();
    let thread_id = thread.id.to_string();
    rsx! { for link in logic::links(&value).into_iter().filter(|link| !badge.as_ref().is_some_and(|badge| link.is(&badge.reference))) {
        { let reference = link.reference(&project_id); let target = Target { thread_id: thread_id.clone(), reference };
          let label = link.state.clone().map(|state| format!(" \u{b7} {state}")).unwrap_or_default();
          let title = if link.watching { format!("{}\n{}", link.title.clone().unwrap_or_else(|| link.url.clone()), logic::WATCHING_HINT) } else { link.title.clone().unwrap_or_else(|| link.url.clone()) };
          rsx! { button { key: "{link.repository}#{link.number}", class: "branch-action pr-badge pr-thread-link", r#type: "button", "aria-label": if link.watching { format!("Linked pull request #{}, watching", link.number) } else { format!("Linked pull request #{}", link.number) }, title,
            onclick: move |_| pull_request_panel::open(state, target.clone()), "#{link.number}{label}" }
            if link.watching && link.host.is_some() && link.state.as_deref().unwrap_or("open") == "open" { StopWatching { state, transport: transport.clone(), thread_id: thread_id.clone(), host: link.host.clone().unwrap_or_default(), repository: link.repository.clone(), number: link.number } } } }
    } }
}

/// "Link to this thread" or "Unlink from this thread" for the pull request the panel shows.
#[component]
pub(crate) fn LinkControl(state: Store<UiModel>, transport: TransportHandle, thread_id: String, reference: Reference, url: String) -> Element {
    let mut pending = use_signal(|| false);
    let supported = state.typed_config().read().as_ref().is_some_and(|config| config.environment.capabilities.thread_pull_request_linking == Some(true));
    let allowed = connected_allows(state, AuthEnvironmentScope::OrchestrationOperate);
    let thread = state.shell().read().snapshot.as_ref().and_then(|snapshot| snapshot.threads.iter().find(|thread| thread.id.as_str() == thread_id)).and_then(|thread| serde_json::to_value(thread).ok());
    let Some(thread) = thread.filter(|_| supported && reference.host.is_some()) else { return rsx! {} };
    let linked = logic::linked_here(&thread, &reference);
    let change = {
        let (transport, thread_id, reference, url) = (transport.clone(), thread_id.clone(), reference.clone(), url.clone());
        move |_| {
            if *pending.peek() {
                return;
            }
            let Some((kind, mut input)) = logic::command(&thread_id, &reference, &url, !linked) else { return };
            pending.set(true);
            input["type"] = json!(kind);
            input["commandId"] = json!(uuid::Uuid::new_v4().to_string());
            let transport = transport.clone();
            spawn(async move {
                let owner = runtime::response_owner(&transport, state);
                let answer = runtime::request_value_reported(transport.clone(), state, "orchestration.dispatchCommand", input).await;
                if owner.is_some() && owner == runtime::response_owner(&transport, state) {
                    if let Err(message) = answer {
                        notice::push(state, Level::Error, if linked { logic::UNLINK_FAILED } else { logic::LINK_FAILED }, message);
                    }
                }
                pending.set(false);
            });
        }
    };
    rsx! { button { class: "pr-link-control", r#type: "button", "aria-label": if linked { logic::UNLINK } else { logic::LINK }, disabled: !allowed || *pending.read(), onclick: change, if linked { "{logic::UNLINK}" } else { "{logic::LINK}" } } }
}

/// "Watch for changes" or "Stop watching" for the pull request the panel shows, when it is open, is
/// filed under this thread and the server can watch pull requests. One write at a time; a refusal is
/// reported and the control comes back.
#[component]
pub(crate) fn WatchControl(state: Store<UiModel>, transport: TransportHandle, thread_id: String, reference: Reference, open: bool) -> Element {
    let mut pending = use_signal(|| false);
    let supported = state.typed_config().read().as_ref().is_some_and(|config| config.environment.capabilities.thread_pull_request_watch == Some(true));
    let allowed = connected_allows(state, AuthEnvironmentScope::OrchestrationOperate);
    let thread = state.shell().read().snapshot.as_ref().and_then(|snapshot| snapshot.threads.iter().find(|thread| thread.id.as_str() == thread_id)).and_then(|thread| serde_json::to_value(thread).ok());
    let link = thread.as_ref().and_then(|thread| logic::links(thread).into_iter().find(|link| link.is(&reference)));
    let (Some(link), Some(host)) = (link.filter(|_| supported && open), reference.host.clone()) else { return rsx! {} };
    let watching = link.watching;
    let change = {
        let (transport, thread_id) = (transport.clone(), thread_id.clone());
        move |_| {
            if *pending.peek() {
                return;
            }
            pending.set(true);
            let mut input = logic::watch_command(&thread_id, &host, &link.repository, link.number, !watching);
            input["commandId"] = json!(uuid::Uuid::new_v4().to_string());
            let transport = transport.clone();
            spawn(async move {
                let owner = runtime::response_owner(&transport, state);
                let answer = runtime::request_value_reported(transport.clone(), state, "orchestration.dispatchCommand", input).await;
                if owner.is_some() && owner == runtime::response_owner(&transport, state) {
                    if let Err(message) = answer {
                        notice::push(state, Level::Error, logic::WATCH_FAILED, message);
                    }
                }
                pending.set(false);
            });
        }
    };
    rsx! { button { class: "pr-link-control", r#type: "button", title: "{logic::WATCHING_HINT}", "aria-pressed": "{watching}", "aria-label": if watching { logic::STOP_WATCHING } else { logic::WATCH }, disabled: !allowed || *pending.read(), onclick: change, if watching { "{logic::STOP_WATCHING}" } else { "{logic::WATCH}" } } }
}

/// The eye on a watched pull request: stops the watch (`Stop watching #N`). One write at a time.
#[component]
fn StopWatching(state: Store<UiModel>, transport: TransportHandle, thread_id: String, host: String, repository: String, number: u64) -> Element {
    let mut pending = use_signal(|| false);
    let allowed = connected_allows(state, AuthEnvironmentScope::OrchestrationOperate);
    rsx! { button { class: "icon-button pr-stop-watching", r#type: "button", "aria-label": "Stop watching #{number}", title: "{logic::WATCHING_HINT}", disabled: !allowed || *pending.read(),
        onclick: move |_| {
            if *pending.peek() {
                return;
            }
            pending.set(true);
            let mut input = logic::watch_command(&thread_id, &host, &repository, number, false);
            input["commandId"] = json!(uuid::Uuid::new_v4().to_string());
            let transport = transport.clone();
            spawn(async move {
                let owner = runtime::response_owner(&transport, state);
                let answer = runtime::request_value_reported(transport.clone(), state, "orchestration.dispatchCommand", input).await;
                if owner.is_some() && owner == runtime::response_owner(&transport, state) {
                    if let Err(message) = answer {
                        notice::push(state, Level::Error, logic::WATCH_FAILED, message);
                    }
                }
                pending.set(false);
            });
        }, "\u{1f441}" } }
}
