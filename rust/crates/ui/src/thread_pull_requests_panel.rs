//! The Pull requests tab of a thread: every pull request filed under it, stacks stepped in under the
//! layer they build on, with the menu of what can be done to one (copy, open, watch, unlink) and the
//! dialog that links one by URL or number. Port of `ThreadPullRequestsPanel.tsx` and
//! `LinkPullRequestDialog.tsx`. The thread's links are the ones its shell carries, so a link made
//! anywhere (by the agent too) shows here without this view asking.
use crate::{
    notice::{self, Level},
    pull_request_panel::{self, Target},
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
    settings_common::connected_allows,
    sidebar_rows::copy_text,
};
use dioxus::prelude::*;
use serde_json::{Value, json};
use t3_client::{
    pull_request::Reference,
    thread_pull_requests::{self as logic, State},
};
use t3_contracts::AuthEnvironmentScope;

/// Shows the tab for a thread; the other panels give way, since they share the margin.
pub(crate) fn open(state: Store<UiModel>, thread_id: &str) {
    state.diff().write().open = false;
    state.pr_panel().set(None);
    state.file_panel().set(None);
    state.thread_prs().set(Some(thread_id.to_owned()));
}
pub(crate) fn close(state: Store<UiModel>) {
    state.thread_prs().set(None);
}
pub(crate) fn toggle(state: Store<UiModel>, thread_id: &str) {
    if state.thread_prs().peek().as_deref() == Some(thread_id) {
        close(state);
    } else {
        open(state, thread_id);
    }
}
/// Whether the environment keeps several links per thread, which is what the tab shows.
pub(crate) fn supported(state: Store<UiModel>) -> bool {
    state.typed_config().read().as_ref().is_some_and(|config| config.environment.capabilities.thread_pull_requests == Some(true))
}
/// Whether the tab has anything to show for the open thread, or can have.
pub(crate) fn thread_value(state: Store<UiModel>, thread_id: &str) -> Option<Value> {
    state.shell().read().snapshot.as_ref().and_then(|snapshot| snapshot.threads.iter().find(|thread| thread.id.as_str() == thread_id)).and_then(|thread| serde_json::to_value(thread).ok())
}

/// Which thread has the link dialog open.
pub(crate) fn open_link_dialog(state: Store<UiModel>, thread_id: &str) {
    state.link_dialog().set(Some(thread_id.to_owned()));
}

#[component]
pub(crate) fn Host(state: Store<UiModel>, transport: TransportHandle, thread_id: String) -> Element {
    let shown = use_memo({
        let thread_id = thread_id.clone();
        move || state.thread_prs().read().as_deref() == Some(thread_id.as_str())
    });
    if !shown() {
        return rsx! {};
    }
    rsx! { Panel { state, transport, thread_id } }
}

fn relative(at: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(at).map(|at| t3_client::thread_status::ago_label(at.with_timezone(&chrono::Utc), chrono::Utc::now())).unwrap_or_default()
}
fn source_label(source: &str) -> &'static str {
    match source {
        "manual" => "Linked by you",
        "created" => "Created from this thread",
        "agent" => "Linked by the agent",
        "stack" => "Found in the stack",
        "stack-dismissed" => "Dismissed",
        _ => "Linked",
    }
}

#[component]
fn Panel(state: Store<UiModel>, transport: TransportHandle, thread_id: String) -> Element {
    let supports = supported(state);
    let value = thread_value(state, &thread_id);
    let operable = connected_allows(state, AuthEnvironmentScope::OrchestrationOperate);
    let watch_supported = state.typed_config().read().as_ref().is_some_and(|config| config.environment.capabilities.thread_pull_request_watch == Some(true));
    let project_id = value.as_ref().and_then(|thread| thread["projectId"].as_str()).unwrap_or_default().to_owned();
    let lines = value.as_ref().map(logic::lines).unwrap_or_default();
    let (open_count, linked, synced) = value.as_ref().map(logic::footer).unwrap_or((0, 0, None));
    let synced_text = synced.as_deref().map(|at| format!(" \u{b7} synced {}", relative(at))).unwrap_or_default();
    let mut menu = use_signal(|| None::<(String, f64, f64)>);
    let close_menu = move || { let mut menu = menu; menu.set(None); };
    let change = {
        let (transport, thread_id) = (transport.clone(), thread_id.clone());
        move |input: Value, failure: &'static str| {
            let transport = transport.clone();
            let _ = &thread_id;
            spawn(async move {
                let owner = runtime::response_owner(&transport, state);
                let answer = runtime::request_value_reported(transport.clone(), state, "orchestration.dispatchCommand", input).await;
                if owner.is_some() && owner == runtime::response_owner(&transport, state) {
                    if let Err(message) = answer {
                        notice::push(state, Level::Error, failure, message);
                    }
                }
            });
        }
    };
    rsx! {
        section { class: "diff-panel thread-prs-panel", role: "region", "aria-label": "Pull requests",
            header { class: "diff-header",
                div { class: "diff-header-main",
                    strong { "Pull requests" }
                    button { class: "icon-button diff-close", "aria-label": "Close pull requests", title: "Close", onclick: move |_| close(state), "\u{00d7}" }
                }
            }
            if !supports {
                div { class: "pr-links-empty",
                    p { strong { "Linked pull requests unavailable" } }
                    p { class: "muted", role: "status", "This environment does not support multiple linked pull requests." }
                }
            } else if lines.is_empty() {
                div { class: "pr-links-empty",
                    p { strong { "No linked pull requests" } }
                    p { class: "muted", "Pull requests the agent opens from this thread land here. Link one yourself from a URL or a number." }
                    button { r#type: "button", "aria-label": "Link a pull request", disabled: !operable, onclick: { let thread_id = thread_id.clone(); move |_| open_link_dialog(state, &thread_id) }, "Link pull request" }
                }
            } else {
                div { class: "pr-links", role: "list", "aria-label": "Linked pull requests",
                    for line in lines.iter() {
                        {
                            let link = &line.link;
                            let number = link["number"].as_u64().unwrap_or(0);
                            let repository = link["repository"].as_str().unwrap_or_default().to_owned();
                            let host = link["host"].as_str().map(str::to_owned);
                            let url = link["url"].as_str().unwrap_or_default().to_owned();
                            let snapshot = &link["snapshot"];
                            let waiting = snapshot.is_null();
                            let pr_state = if waiting { State::Open } else { State::of(snapshot["state"].as_str().unwrap_or("open"), snapshot["isDraft"].as_bool().unwrap_or(false)) };
                            let is_open = waiting || snapshot["state"] == "open";
                            let watching = link["watch"].is_object();
                            let title = snapshot["title"].as_str().map(str::to_owned).unwrap_or_else(|| repository.clone());
                            let reference = Reference::from_link(&project_id, link);
                            let key = format!("{}/{repository}#{number}", host.clone().unwrap_or_default());
                            let tip = format!("{} \u{b7} {}", source_label(link["source"].as_str().unwrap_or_default()), relative(link["linkedAt"].as_str().unwrap_or_default()));
                            let (additions, deletions) = (snapshot["additions"].as_u64().unwrap_or(0), snapshot["deletions"].as_u64().unwrap_or(0));
                            let target = reference.clone().map(|reference| Target { thread_id: thread_id.clone(), reference });
                            let pad = 0.5 + line.depth.min(3) as f64 * 1.25;
                            let stack = line.stack;
                            let open_row = { let target = target.clone(); move |_| { if let Some(target) = target.clone() { pull_request_panel::open(state, target); } } };
                            let stack_label = stack.map(|(native, size)| if native { format!("GitHub stack of {size}: merging a layer lands the ones below it.") } else { format!("{size} pull requests chained by base branch.") });
                            let author = snapshot["author"]["login"].as_str().unwrap_or_default().to_owned();
                            let menu_key = key.clone();
                            rsx! {
                                div { key: "{key}", class: "pr-link-row", role: "listitem", style: "padding-left:{pad}rem",
                                    oncontextmenu: { let menu_key = menu_key.clone(); move |event| { event.prevent_default(); event.stop_propagation(); let at = event.client_coordinates(); menu.set(Some((menu_key.clone(), at.x, at.y))); } },
                                    if line.depth > 0 { span { class: "pr-link-connector", "aria-hidden": "true" } }
                                    crate::pull_request_glyph::Glyph { state: pr_state, stack: false }
                                    button { class: "pr-link-main", r#type: "button", "aria-label": "Open pull request #{number}", title: "{url}", onclick: open_row,
                                        span { class: "pr-link-line",
                                            span { class: "pr-link-number", title: "{tip}", "#{number}" }
                                            span { class: "pr-link-title", "{title}" }
                                            if is_open && watching { span { class: "pr-link-signal", role: "img", "aria-label": "Watching", title: "{logic::WATCHING_HINT}", "\u{25c9}" } }
                                            if is_open { if let Some(text) = t3_client::pull_request::checks_state_label(snapshot["checksState"].as_str()) { span { class: "pr-link-signal muted", "{text}" } } }
                                            if is_open { if let Some(text) = t3_client::pull_request::review_decision_label(snapshot["reviewDecision"].as_str()) { span { class: "pr-link-signal muted", "{text}" } } }
                                            if t3_client::pull_request::has_size(additions, deletions) { span { class: "pr-link-stat", "+{additions} \u{2212}{deletions}" } }
                                        }
                                        span { class: "pr-link-line muted",
                                            if let Some(label) = stack_label { span { class: "pr-link-stack", title: "{label}", crate::pull_request_glyph::Glyph { state: State::Open, stack: true } "{stack.map(|(_, size)| size).unwrap_or(0)}" } }
                                            if !author.is_empty() { span { "{author}" } }
                                            if waiting { span { "{host.clone().unwrap_or_default()}/{repository}" } }
                                            else {
                                                span { class: "pr-link-repository", "{repository}" }
                                                span { class: "pr-link-branches", "{snapshot[\"headBranch\"].as_str().unwrap_or_default()} \u{2192} {snapshot[\"baseBranch\"].as_str().unwrap_or_default()}" }
                                            }
                                            if let Some(at) = snapshot["updatedAt"].as_str() { span { "{relative(at)}" } }
                                        }
                                    }
                                    button { class: "icon-button pr-link-actions", r#type: "button", "aria-label": "Actions for #{number}", "aria-haspopup": "menu", onclick: { let menu_key = menu_key.clone(); move |event| { let at = event.client_coordinates(); menu.set(Some((menu_key.clone(), at.x, at.y))); } }, "\u{22ef}" }
                                    if menu.read().as_ref().is_some_and(|(open, ..)| *open == menu_key) {
                                        {
                                            let (x, y) = menu.read().as_ref().map(|(_, x, y)| (*x, *y)).unwrap_or_default();
                                            let thread_id = thread_id.clone();
                                            let (host_for_menu, repository_for_menu) = (host.clone(), repository.clone());
                                            let dismissing = link["source"] == "stack";
                                            let open_target = target.clone();
                                            let change = change.clone();
                                            let url_for_menu = url.clone();
                                            rsx! {
                                                div { class: "menu-backdrop", onclick: move |_| close_menu(), oncontextmenu: move |event| { event.prevent_default(); close_menu(); } }
                                                div { class: "context-menu", role: "menu", "aria-label": "Pull request actions", style: "position:fixed;left:{x}px;top:{y}px", tabindex: "0",
                                                    onmounted: move |event| { drop(event.set_focus(true)); },
                                                    onkeydown: move |event| { if event.key() == Key::Escape { event.stop_propagation(); close_menu(); } },
                                                    button { class: "menu-item", role: "menuitem", r#type: "button", "aria-label": "Copy link", onclick: { let url = url_for_menu.clone(); move |_| { close_menu(); copy_text(&url); notice::push(state, Level::Success, "Link copied", url.clone()); } }, "Copy link" }
                                                    button { class: "menu-item", role: "menuitem", r#type: "button", "aria-label": "Open", disabled: open_target.is_none(), onclick: move |_| { close_menu(); if let Some(target) = open_target.clone() { pull_request_panel::open(state, target); } }, "Open" }
                                                    if watch_supported && is_open {
                                                        button { class: "menu-item", role: "menuitem", r#type: "button", "aria-label": if watching { "Stop watching" } else { "Watch for changes" }, disabled: !operable || host_for_menu.is_none(),
                                                            onclick: { let (change, thread_id, host, repository) = (change.clone(), thread_id.clone(), host_for_menu.clone(), repository_for_menu.clone());
                                                                move |_| { close_menu(); if let Some(host) = host.clone() { change(logic::watch_command(&thread_id, &host, &repository, number, !watching), "Could not change watching"); } } },
                                                            if watching { "Stop watching" } else { "Watch for changes" } }
                                                    }
                                                    button { class: "menu-item", role: "menuitem", r#type: "button", "aria-label": if dismissing { "Dismiss from thread" } else { "Unlink from thread" }, disabled: !operable,
                                                        onclick: { let (change, thread_id, reference) = (change.clone(), thread_id.clone(), reference.clone());
                                                            move |_| { close_menu(); if let Some((kind, mut input)) = reference.as_ref().and_then(|reference| logic::command(&thread_id, reference, "", false)) { input["type"] = json!(kind); input["commandId"] = json!(uuid::Uuid::new_v4().to_string()); change(input, logic::UNLINK_FAILED); } } },
                                                        if dismissing { "Dismiss from thread" } else { "Unlink from thread" } }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                footer { class: "pr-links-footer",
                    span { role: "status", "{open_count} open \u{b7} {linked} linked{synced_text}" }
                    button { r#type: "button", "aria-label": "Link a pull request", disabled: !operable, onclick: { let thread_id = thread_id.clone(); move |_| open_link_dialog(state, &thread_id) }, "Link" }
                }
            }
        }
    }
}

/// The web address of pull request `number` in the project's own repository, from the project's repository identity.
fn own_web_url(identity: &Option<Value>, number: u64) -> Option<String> {
    let identity = identity.as_ref()?;
    let remote = identity["locator"]["remoteUrl"].as_str()?;
    let repository = identity["displayName"].as_str().map(str::to_owned).or_else(|| Some(format!("{}/{}", identity["owner"].as_str()?, identity["name"].as_str()?)))?;
    let host = remote.trim_start_matches("https://").trim_start_matches("http://").trim_start_matches("git@").split(['/', ':']).next()?.to_owned();
    let segment = if remote.contains("gitlab") { "-/merge_requests" } else { "pull" };
    Some(format!("https://{host}/{repository}/{segment}/{number}"))
}

/// The dialog that links a pull request to a thread by URL, `#42` or `42`, rendered once by the app.
#[component]
pub(crate) fn LinkDialog(state: Store<UiModel>, transport: TransportHandle) -> Element {
    let Some(thread_id) = state.link_dialog().read().clone() else { return rsx! {} };
    rsx! { Link { key: "{thread_id}", state, transport, thread_id } }
}
#[component]
fn Link(state: Store<UiModel>, transport: TransportHandle, thread_id: String) -> Element {
    let mut reference = use_signal(String::new);
    let mut dirty = use_signal(|| false);
    let mut pending = use_signal(|| false);
    let mut failure = use_signal(|| None::<String>);
    let thread = thread_value(state, &thread_id);
    let project_id = thread.as_ref().and_then(|thread| thread["projectId"].as_str()).unwrap_or_default().to_owned();
    // A bare number can only mean the thread's own repository: the project's web address for it.
    let own = state.shell().read().snapshot.as_ref().and_then(|snapshot| snapshot.projects.iter().find(|project| project.id.as_str() == project_id)).and_then(|project| project.extra.get("repositoryIdentity").cloned());
    let typed = reference();
    let resolved = logic::resolve_link_input(&typed, |number| own_web_url(&own, number));
    let validation = if !dirty() {
        None
    } else if typed.trim().is_empty() {
        Some("Paste a pull request URL or enter 123 / #123.".to_owned())
    } else {
        match &resolved {
            None => Some("Use a pull request URL, 123, or #123.".to_owned()),
            Some(Err(message)) => Some(message.clone()),
            Some(Ok(_)) => None,
        }
    };
    let close = move || state.link_dialog().set(None);
    let submit = {
        let (transport, thread_id, project_id, own) = (transport.clone(), thread_id.clone(), project_id.clone(), own.clone());
        move || {
            dirty.set(true);
            if *pending.peek() {
                return;
            }
            let Some(Ok(named)) = logic::resolve_link_input(&reference.peek(), |number| own_web_url(&own, number)) else { return };
            failure.set(None);
            pending.set(true);
            let reference_value = Reference { project_id: project_id.clone(), host: Some(named.host.clone()), repository: named.repository.clone(), number: named.number };
            let Some((kind, mut input)) = logic::command(&thread_id, &reference_value, &named.url, true) else { return };
            input["type"] = json!(kind);
            input["commandId"] = json!(uuid::Uuid::new_v4().to_string());
            let transport = transport.clone();
            spawn(async move {
                let owner = runtime::response_owner(&transport, state);
                let answer = runtime::request_value_reported(transport.clone(), state, "orchestration.dispatchCommand", input).await;
                pending.set(false);
                if owner.is_none() || owner != runtime::response_owner(&transport, state) {
                    return;
                }
                match answer {
                    Ok(_) => state.link_dialog().set(None),
                    Err(message) => failure.set(Some(if message.is_empty() { "Could not link the pull request.".to_owned() } else { message })),
                }
            });
        }
    };
    let ready = matches!(resolved, Some(Ok(_)));
    rsx! {
        div { class: "modal-backdrop", onclick: move |_| { if !pending() { close() } },
            div { class: "dialog", role: "dialog", "aria-modal": "true", "aria-label": "Link pull request", onclick: move |event| event.stop_propagation(),
                onkeydown: move |event| { if event.key() == Key::Escape && !pending() { event.stop_propagation(); close(); } },
                h2 { "Link pull request" }
                p { class: "muted", "Attach a pull request to this thread. A full URL can point at any repository on a host this environment has a project for." }
                input { "aria-label": "Pull request URL or #42", placeholder: "Pull request URL or #42", value: "{typed}", onmounted: move |event| { drop(event.set_focus(true)); },
                    oninput: move |event| { dirty.set(true); reference.set(event.value()); },
                    onkeydown: { let mut submit = submit.clone(); move |event| { if event.key() == Key::Enter { event.prevent_default(); submit(); } } } }
                if let Some(Ok(named)) = &resolved { p { class: "muted", "{named.host}/{named.repository} #{named.number}" } }
                if let Some(message) = validation.or_else(|| failure.read().clone()) { p { class: "error-banner", role: "alert", "{message}" } }
                div { class: "dialog-actions",
                    button { r#type: "button", "aria-label": "Cancel", disabled: pending(), onclick: move |_| close(), "Cancel" }
                    button { class: "primary", r#type: "button", "aria-label": "Confirm link", disabled: pending() || !ready, onclick: { let mut submit = submit.clone(); move |_| submit() }, if pending() { "Linking..." } else { "Link" } }
                }
            }
        }
    }
}

/// The header's button for the tab, offered where the environment keeps several links per thread.
#[component]
pub(crate) fn Toggle(state: Store<UiModel>, thread_id: String) -> Element {
    if !supported(state) {
        return rsx! {};
    }
    let open = state.thread_prs().read().as_deref() == Some(thread_id.as_str());
    rsx! { button { class: if open { "icon-button selected" } else { "icon-button" }, "aria-label": "Pull requests", "aria-pressed": "{open}", title: "Pull requests linked to this thread", onclick: move |_| toggle(state, &thread_id), "PRs" } }
}
