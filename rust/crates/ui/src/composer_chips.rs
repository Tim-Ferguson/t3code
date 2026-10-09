//! The chips on a draft: a terminal selection, a review comment on diff lines, another thread.
//! Port of the composer's context chips (`ComposerPendingTerminalContexts.tsx`,
//! `composerContextPresentation.tsx`) as a tray above the message box. A chip belongs to one
//! draft, is saved with it, and travels as a `context` record on the message that sends it; the
//! references that place it in the text are added after the prose at send time.
//!
//! Adding or removing a chip writes only the chips of that draft, so only the composer that
//! shows them repaints.
use crate::runtime::{UiModel, UiModelStoreExt};
use dioxus::prelude::*;
use serde_json::Value;
use t3_client::{composer_context as logic, draft_storage::DraftTarget};

/// The chips of a draft: this session's list, else the saved one.
pub(crate) fn contexts_of(state: Store<UiModel>, target: &DraftTarget) -> Vec<Value> {
    if let Some(list) = state.draft_contexts().peek().get(target) {
        return list.clone();
    }
    state.peek().draft_storage.document.borrow().contexts(target)
}

/// The draft the person is composing in: the open thread's, else the new-thread form's project.
pub(crate) fn open_target(state: Store<UiModel>) -> Option<DraftTarget> {
    let model = state.peek();
    let environment = model.destination.as_ref()?.to_string();
    match (&model.active_thread, &model.selected_project) {
        (Some(thread), _) => Some(DraftTarget::thread(environment, thread.clone())),
        (None, Some(project)) => Some(DraftTarget::project(environment, project.clone())),
        _ => None,
    }
}

pub(crate) fn edit(state: Store<UiModel>, target: DraftTarget, list: Vec<Value>) {
    crate::draft_storage::edit_contexts(state, target.clone(), list.clone());
    state.draft_contexts().write().insert(target, list);
}
/// Adds a chip, or replaces the one with the same identity (a thread attached twice is one chip).
pub(crate) fn add(state: Store<UiModel>, target: DraftTarget, record: Value) {
    let mut list = contexts_of(state, &target);
    match list.iter().position(|existing| existing["contextId"] == record["contextId"]) {
        Some(at) => list[at] = record,
        None => list.push(record),
    }
    edit(state, target, list);
}
pub(crate) fn remove(state: Store<UiModel>, target: DraftTarget, context_id: &str) {
    let list: Vec<Value> = contexts_of(state, &target).into_iter().filter(|record| record["contextId"] != context_id).collect();
    edit(state, target, list);
}

/// What a send claims from the draft: the text with its references, the context, and the ids.
pub(crate) fn for_send(state: Store<UiModel>, target: &DraftTarget, prompt: &str) -> (String, Option<Value>, Vec<String>) {
    let records = contexts_of(state, target);
    let ids = records.iter().filter_map(|record| record["contextId"].as_str().map(str::to_owned)).collect();
    let (text, context) = logic::compose(prompt, &records);
    (text, context, ids)
}
/// A send was accepted: its chips leave the draft.
pub(crate) fn sent(state: Store<UiModel>, target: &DraftTarget, ids: &[String]) {
    if ids.is_empty() {
        return;
    }
    let list: Vec<Value> = contexts_of(state, target).into_iter().filter(|record| !record["contextId"].as_str().is_some_and(|id| ids.iter().any(|sent| sent == id))).collect();
    edit(state, target.clone(), list);
}

fn tooltip(record: &Value) -> String {
    match record["kind"].as_str() {
        Some("terminal") => format!("{}, {} lines selected", record["label"].as_str().unwrap_or(""), record["text"].as_str().map_or(0, |text| text.lines().count())),
        Some("review-comment") => format!("{}: {}", record["filePath"].as_str().unwrap_or(""), record["text"].as_str().unwrap_or("")),
        Some("thread") => format!("Thread {}", record["title"].as_str().unwrap_or("")),
        _ => record["label"].as_str().unwrap_or("").to_owned(),
    }
}

/// The chips of a draft with a button to remove each. Nothing is drawn without any.
#[component]
pub(crate) fn Tray(state: Store<UiModel>, target: DraftTarget) -> Element {
    let list = match state.draft_contexts().read().get(&target) {
        Some(list) => list.clone(),
        None => state.peek().draft_storage.document.borrow().contexts(&target),
    };
    if list.is_empty() {
        return rsx! {};
    }
    rsx! { ul { class: "context-chips", "aria-label": "Context",
        for record in list {
            {
                let id = record["contextId"].as_str().unwrap_or("").to_owned();
                let label = record["label"].as_str().unwrap_or("").to_owned();
                let kind = logic::kind_label(record["kind"].as_str().unwrap_or(""));
                let title = tooltip(&record);
                rsx! { li { key: "{id}", class: "context-chip", title: "{title}",
                    span { class: "context-chip-kind muted", "{kind}" }
                    span { class: "context-chip-label", "{label}" }
                    button { r#type: "button", class: "context-chip-remove", "aria-label": "Remove {label}", onclick: { let target = target.clone(); move |_| remove(state, target.clone(), &id) }, "\u{d7}" }
                } }
            }
        }
    } }
}

/// How many chips a draft has, for whoever decides whether it can be sent.
pub(crate) fn count(state: Store<UiModel>, target: &DraftTarget) -> usize {
    match state.draft_contexts().read().get(target) {
        Some(list) => list.len(),
        None => state.peek().draft_storage.document.borrow().contexts(target).len(),
    }
}
