//! The hand-offs of the pull request panel's "More pull request actions" menu: ask a question about the
//! pull request, have it explained, or have its open findings fixed. Each leaves its task in a composer
//! for the reader to read and send, and sends nothing itself. Port of `startAsk` and `writeTaskToComposer`
//! of `PullRequestDetailPanel.tsx`; what a task holds is `t3_client::pull_request_handoff`.
//!
//! In a thread the task goes to that thread's composer. From the pull requests page there is no thread, so
//! the project's new-thread form is opened and the task goes there, after the pull request is checked out
//! into a worktree of its own and the form points at it (`startHandoff`). In a thread the task goes to the
//! thread's composer and nothing is checked out. The page header's "Check out" menu is `check_out` alone.
use crate::{
    composer_chips,
    notice::{self, Level},
    pull_request_panel::Read,
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
};
use dioxus::prelude::*;
use serde_json::Value;
use std::{cell::RefCell, collections::BTreeMap, rc::Rc};
use t3_client::{composer_context, pull_request_handoff as logic};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Ask,
    Explain,
    FixFindings,
}

thread_local! {
    /// What the last hand-off put in each draft: its sentence, and the chips (by `contextId`) that are its own.
    /// A hand-off owns what an earlier hand-off wrote and nothing else.
    static LAST: RefCell<BTreeMap<String, (String, Vec<String>)>> = const { RefCell::new(BTreeMap::new()) };
}

fn task(kind: Kind, value: &Value, activity: &Read<Rc<Value>>) -> logic::Handoff {
    match kind {
        Kind::Ask => logic::ask(value),
        Kind::Explain => logic::explain(value),
        Kind::FixFindings => {
            let (threads, comments, truncated) = match activity {
                Read::Ready(activity) => (activity["reviewThreads"].as_array().cloned().unwrap_or_default(), activity["comments"].as_array().cloned().unwrap_or_default(), activity["commentsTruncated"] == true),
                _ => (vec![], vec![], false),
            };
            logic::fix_findings(value, &threads, &comments, &value["checks"].as_array().cloned().unwrap_or_default(), truncated)
        }
    }
}

/// Puts the task in the composer: the open thread's when `in_thread`, else the project's new-thread form.
pub(crate) fn deliver(state: Store<UiModel>, transport: &TransportHandle, project_id: &str, in_thread: bool, kind: Kind, value: &Value, activity: &Read<Rc<Value>>) {
    let handoff = task(kind, value, activity);
    if !in_thread {
        runtime::new_thread(transport, state, Some(project_id.to_owned()));
    }
    let Some(target) = composer_chips::open_target(state) else { return };
    let key = format!("{target:?}");
    let (last_prompt, last_chips) = LAST.with(|last| last.borrow().get(&key).cloned()).unwrap_or_default();
    let (existing, set): (String, Box<dyn Fn(String)>) = if in_thread {
        (state.draft().peek().clone(), Box::new(move |prompt| crate::draft_storage::edit_thread_prompt(state, prompt)))
    } else {
        let Some(environment) = state.destination().peek().clone() else { return };
        let key = (environment, target.local_id.clone());
        (state.new_thread_drafts().peek().get(&key).cloned().unwrap_or_default(), Box::new(move |prompt| crate::draft_storage::edit_draft_prompt(state, key.clone(), prompt)))
    };
    set(logic::merged_prompt(&existing, Some(&last_prompt).filter(|last| !last.is_empty()).map(String::as_str), &handoff.prompt));
    // The chips of an earlier hand-off go; a chip the reader attached stays.
    for id in &last_chips {
        composer_chips::remove(state, target.clone(), id);
    }
    let mut placed = Vec::new();
    for comment in &handoff.comments {
        let record = composer_context::review_record(comment);
        if let Some(id) = record["contextId"].as_str() {
            placed.push(id.to_owned());
        }
        composer_chips::add(state, target.clone(), record);
    }
    LAST.with(|last| last.borrow_mut().insert(key, (handoff.prompt.clone(), placed)));
    let (title, detail) = match (kind, in_thread) {
        (Kind::Ask, true) => ("Added to the composer", "The pull request is in the composer \u{2014} type your question, then send."),
        (Kind::Ask, false) => ("Asked in a thread", "The pull request is in the composer \u{2014} type your question, then send."),
        (Kind::Explain, _) => ("Added to the composer", "The request is in the composer \u{2014} read it over, then send."),
        (Kind::FixFindings, true) => ("Added to the composer", "The findings are in the composer \u{2014} read them over, then send. Nothing was checked out."),
        (Kind::FixFindings, false) => ("Added to the composer", "The findings are in the composer \u{2014} read them over, then send."),
    };
    notice::push(state, Level::Success, title, detail);
}

/// Checks the pull request out and opens a thread on it, then (for a hand-off) puts its task in that thread's composer
/// (`startHandoff`). The thread comes first, so the project's setup runs for a checkout that knows its thread; a worktree
/// leaves whatever is open alone, a local checkout moves the branch of the repository itself.
pub(crate) fn check_out(state: Store<UiModel>, transport: &TransportHandle, project_id: &str, value: &Value, mode: &'static str, then: Option<(Kind, Read<Rc<Value>>)>) {
    let Some(cwd) = state.shell().peek().snapshot.as_ref().and_then(|snapshot| snapshot.projects.iter().find(|project| project.id.as_str() == project_id).map(|project| project.workspace_root.to_string())) else { return };
    let Some(reference) = value["url"].as_str().map(str::to_owned) else { return };
    let loading = notice::push_with(state, Level::Info, "Preparing the pull request checkout...", "", None);
    runtime::new_thread(transport, state, Some(project_id.to_owned()));
    let mut input = serde_json::json!({"cwd":cwd,"reference":reference,"mode":mode});
    if mode == "worktree" {
        input["threadId"] = serde_json::json!(format!("thread-{}", uuid::Uuid::new_v4()));
    }
    let (transport, project_id, value) = (transport.clone(), project_id.to_owned(), value.clone());
    // Not tied to the menu that started it: opening the new thread unmounts the panel this runs from.
    dioxus::dioxus_core::spawn_forever(async move {
        let owner = runtime::response_owner(&transport, state);
        let answer = runtime::request_value_reported(transport.clone(), state, "git.preparePullRequestThread", input).await;
        if owner.is_none() || owner != runtime::response_owner(&transport, state) {
            return;
        }
        let done = match answer {
            Ok(done) => done,
            // The server says what to do about it (the branch is already checked out in the main repository, say).
            Err(message) => {
                notice::replace(state, loading, Level::Error, "Could not prepare the pull request checkout", message);
                return;
            }
        };
        runtime::new_thread(&transport, state, Some(project_id.clone()));
        crate::new_thread::apply_prepared(state, done["branch"].as_str().unwrap_or_default(), done["worktreePath"].as_str().map(str::to_owned));
        let on_head = done["isOnPullRequestHead"].as_bool().unwrap_or(true);
        match then {
            Some((kind, activity)) => {
                notice::dismiss(state, loading);
                deliver(state, &transport, &project_id, false, kind, &value, &activity);
                if !on_head {
                    notice::push(state, Level::Warning, "Checked out, but not on the latest commits", "The checkout could not be moved onto the pull request's latest commits, so the code there is older than the pull request.");
                }
            }
            None if on_head => notice::replace(state, loading, Level::Success, if mode == "local" { "Checked out here" } else { "Checked out" }, if mode == "local" { "This repository is on the pull request's branch, with a thread open on it." } else { "The pull request is in its own worktree, with a thread open on it." }),
            None => notice::replace(state, loading, Level::Warning, "Checked out, but not on the latest commits", "The checkout could not be moved onto the pull request's latest commits, so the code there is older than the pull request."),
        }
    });
}

/// The page header's "Check out" menu: the pull request in a worktree of its own, or in the repository itself.
#[component]
pub(crate) fn CheckoutMenu(state: Store<UiModel>, transport: TransportHandle, project_id: String, value: Rc<Value>) -> Element {
    let mut open = use_signal(|| false);
    let write = crate::settings_common::connected_allows(state, t3_contracts::AuthEnvironmentScope::SourceControlWrite);
    let operate = crate::settings_common::connected_allows(state, t3_contracts::AuthEnvironmentScope::OrchestrationOperate);
    let item = |mode: &'static str, label: &'static str, hint: &'static str, allowed: bool| {
        let (transport, project_id, value) = (transport.clone(), project_id.clone(), value.clone());
        rsx! { button { class: "menu-item", role: "menuitem", r#type: "button", "aria-label": "{label}", disabled: !allowed, onclick: move |_| { open.set(false); check_out(state, &transport, &project_id, &value, mode, None); },
            span { "{label}" } span { class: "muted menu-hint", "{hint}" } } }
    };
    rsx! {
        div { class: "project-menu",
            button { r#type: "button", "aria-label": "Check out", "aria-haspopup": "menu", "aria-expanded": "{open()}", title: "Check out this pull request", disabled: !write, onclick: move |_| open.set(!open()), "Check out \u{25be}" }
            if open() {
                div { class: "menu-backdrop", onclick: move |_| open.set(false) }
                div { class: "context-menu", role: "menu", "aria-label": "Check out this pull request", tabindex: "0", style: "right:0;left:auto;",
                    onmounted: move |event| { drop(event.set_focus(true)); },
                    onkeydown: move |event| { if event.key() == Key::Escape { event.stop_propagation(); open.set(false); } },
                    { item("worktree", "In a separate worktree", "Its own folder and thread. Nothing you have open moves.", write && operate) }
                    { item("local", "In this repository", "Switches the branch you are working in, like `gh pr checkout`.", write) }
                }
            }
        }
    }
}

/// The menu: three hand-offs, each said in the original's words.
#[component]
pub(crate) fn Menu(state: Store<UiModel>, transport: TransportHandle, project_id: String, thread_id: String, value: Rc<Value>, activity: Read<Rc<Value>>) -> Element {
    let mut open = use_signal(|| false);
    let in_thread = !thread_id.is_empty() && state.active_thread().read().as_deref() == Some(thread_id.as_str());
    let can = *state.status().read() == t3_client::connection::ConnectionStatus::Connected;
    let item = |kind: Kind, label: &'static str, hint: &'static str| {
        let (transport, project_id, value, activity) = (transport.clone(), project_id.clone(), value.clone(), activity.clone());
        rsx! { button { class: "menu-item", role: "menuitem", r#type: "button", "aria-label": "{label}", disabled: !can, onclick: move |_| { open.set(false); if !in_thread && kind != Kind::Ask { check_out(state, &transport, &project_id, &value, "worktree", Some((kind, activity.clone()))); } else { deliver(state, &transport, &project_id, in_thread, kind, &value, &activity); } },
            span { "{label}" } span { class: "muted menu-hint", "{hint}" } } }
    };
    rsx! {
        div { class: "project-menu",
            button { class: "icon-button", r#type: "button", "aria-label": "More pull request actions", "aria-haspopup": "menu", "aria-expanded": "{open()}", title: "More pull request actions", onclick: move |_| open.set(!open()), "\u{22ef}" }
            if open() {
                div { class: "menu-backdrop", onclick: move |_| open.set(false) }
                div { class: "context-menu", role: "menu", "aria-label": "More pull request actions", tabindex: "0", style: "right:0;left:auto;",
                    onmounted: move |event| { drop(event.set_focus(true)); },
                    onkeydown: move |event| { if event.key() == Key::Escape { event.stop_propagation(); open.set(false); } },
                    { item(Kind::Ask, "Ask a question", if in_thread { "Adds the pull request to this thread's composer." } else { "Opens a thread that knows which pull request you mean." }) }
                    { item(Kind::Explain, "Explain this PR", "A walk through the diff and what to read closely.") }
                    { item(Kind::FixFindings, if in_thread { "Fix findings in this thread" } else { "Fix findings in a thread" }, "Hands the open review threads and failing checks over.") }
                }
            }
        }
    }
}
