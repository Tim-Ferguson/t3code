//! The hand-offs of the pull request panel's "More pull request actions" menu: ask a question about the
//! pull request, have it explained, or have its open findings fixed. Each leaves its task in a composer
//! for the reader to read and send, and sends nothing itself. Port of `startAsk` and `writeTaskToComposer`
//! of `PullRequestDetailPanel.tsx`; what a task holds is `t3_client::pull_request_handoff`.
//!
//! In a thread the task goes to that thread's composer. From the pull requests page there is no thread, so
//! the project's new-thread form is opened and the task goes there. The original checks the pull request
//! out into a worktree first for the fix; this does not, and says so in the toast.
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
        let key = (environment, project_id.to_owned());
        (state.new_thread_drafts().peek().get(&key).cloned().unwrap_or_default(), Box::new(move |prompt| crate::draft_storage::edit_project_prompt(state, key.clone(), prompt)))
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
        (Kind::FixFindings, _) => ("Added to the composer", "The findings are in the composer \u{2014} read them over, then send. Nothing was checked out."),
    };
    notice::push(state, Level::Success, title, detail);
}

/// The menu: three hand-offs, each said in the original's words.
#[component]
pub(crate) fn Menu(state: Store<UiModel>, transport: TransportHandle, project_id: String, thread_id: String, value: Rc<Value>, activity: Read<Rc<Value>>) -> Element {
    let mut open = use_signal(|| false);
    let in_thread = !thread_id.is_empty() && state.active_thread().read().as_deref() == Some(thread_id.as_str());
    let can = *state.status().read() == t3_client::connection::ConnectionStatus::Connected;
    let item = |kind: Kind, label: &'static str, hint: &'static str| {
        let (transport, project_id, value, activity) = (transport.clone(), project_id.clone(), value.clone(), activity.clone());
        rsx! { button { class: "menu-item", role: "menuitem", r#type: "button", "aria-label": "{label}", disabled: !can, onclick: move |_| { open.set(false); deliver(state, &transport, &project_id, in_thread, kind, &value, &activity); },
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
