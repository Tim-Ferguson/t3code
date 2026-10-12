//! Several drafts of new threads, each at its own address (`/draft/<id>`), as the original keeps them
//! (`composerDraftStore.ts`, `useHandleNewThread.ts`, the draft rows of `Sidebar.tsx`).
//!
//! A draft belongs to a project, holds the text, attachments, chips and choices of a thread that has not
//! started, and goes when its thread starts or when the person discards it. A new-thread request reuses a draft
//! of the project that holds nothing and starts a new one when the draft it would reuse holds work: the work
//! stays in the sidebar, one row per draft, until it is opened again, sent or discarded.
//!
//! Where drafts are kept is `t3_client::draft_storage`; this module keeps the model (`active_draft`) in step.
use crate::runtime::{self, UiModel, UiModelStoreExt, View};
use dioxus::prelude::*;
use t3_client::draft_storage::{DiscardedDraft, DraftTarget};
use t3_contracts::EnvironmentId;

/// A draft with work in it, as the sidebar lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub environment: EnvironmentId,
    pub draft_id: String,
    pub project_id: String,
    pub preview: String,
}

/// A discard that can still be taken back.
#[derive(Debug, Clone)]
pub struct Discarded {
    pub token: u64,
    pub saved: DiscardedDraft,
}

/// How long a discard can be taken back, as an archive can.
const UNDO_MILLISECONDS: u64 = 5_000;

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn bump(state: Store<UiModel>) {
    let next = *state.draft_revision().peek() + 1;
    state.draft_revision().set(next);
}

fn target(destination: &EnvironmentId, draft_id: &str) -> DraftTarget {
    DraftTarget::draft(destination.to_string(), draft_id)
}

/// The project of a draft of this environment, when the draft is still open.
pub(crate) fn project_of(state: Store<UiModel>, draft_id: &str) -> Option<String> {
    let model = state.peek();
    let destination = model.destination.as_ref()?;
    model.draft_storage.document.borrow().draft(&target(destination, draft_id)).map(|session| session.project_id)
}

/// Whether the draft holds work the person would miss.
pub(crate) fn has_content(state: Store<UiModel>, draft_id: &str) -> bool {
    let model = state.peek();
    model.destination.as_ref().is_some_and(|destination| model.draft_storage.document.borrow().has_content(&target(destination, draft_id)))
}

/// `ensure_active` for the project the form is on: the one picked, else the first.
pub(crate) fn ensure_for_form(state: Store<UiModel>) {
    let project = state.selected_project().peek().clone().or_else(|| state.shell().peek().snapshot.as_ref().and_then(|snapshot| snapshot.projects.first()).map(|project| project.id.to_string()));
    if let Some(project) = project.filter(|_| *state.view().peek() == View::Chat && state.active_thread().peek().is_none()) {
        ensure_active(state, &project);
    }
}

/// Gives the start form a draft of its project: the one it shows if that is the project's, else a draft of the project that holds
/// nothing, else a new one. A draft left behind with nothing in it goes with the move.
pub(crate) fn ensure_active(state: Store<UiModel>, project_id: &str) {
    let Some(destination) = state.destination().peek().clone() else { return };
    let previous = state.active_draft().peek().clone();
    if previous.as_deref().and_then(|id| project_of(state, id)).as_deref() == Some(project_id) {
        return;
    }
    let chosen = {
        let model = state.peek();
        let document = model.draft_storage.document.borrow();
        document
            .drafts()
            .into_iter()
            .find(|session| session.environment == destination.as_str() && session.project_id == project_id && !document.has_content(&session.target()))
            .map(|session| session.draft_id)
    };
    let chosen = chosen.unwrap_or_else(|| {
        let id = uuid::Uuid::new_v4().to_string();
        state.peek().draft_storage.document.borrow_mut().start_draft(destination.as_str(), &id, project_id, &now());
        crate::draft_storage::schedule(state);
        id
    });
    if let Some(previous) = previous.filter(|previous| *previous != chosen) {
        retire_if_empty(state, &previous);
    }
    state.active_draft().set(Some(chosen));
    bump(state);
}

/// A draft the form has moved on from, with nothing in it, is not worth keeping.
pub(crate) fn retire_if_empty(state: Store<UiModel>, draft_id: &str) {
    let Some(destination) = state.destination().peek().clone() else { return };
    if project_of(state, draft_id).is_some() && !has_content(state, draft_id) {
        forget_draft(state, &destination, draft_id);
    }
}

/// Takes a draft out of the saved ones and out of memory; what it held is not recoverable. For a draft that is empty or has been sent.
pub(crate) fn forget_draft(state: Store<UiModel>, destination: &EnvironmentId, draft_id: &str) -> Option<DiscardedDraft> {
    let target = target(destination, draft_id);
    let discarded = state.peek().draft_storage.document.borrow_mut().discard(&target);
    let key = (destination.clone(), draft_id.to_owned());
    state.new_thread_drafts().write().remove(&key);
    state.new_thread_choices().write().remove(&key);
    state.draft_attachments().write().remove(&target);
    state.draft_contexts().write().remove(&target);
    crate::draft_storage::schedule(state);
    bump(state);
    discarded
}

/// The draft's thread has started: the draft is gone, and the form, if it showed this draft, takes another.
pub(crate) fn promote(state: Store<UiModel>, destination: &EnvironmentId, draft_id: &str) {
    forget_draft(state, destination, draft_id);
    if state.active_draft().peek().as_deref() == Some(draft_id) {
        state.active_draft().set(None);
    }
}

/// Discards a draft of any environment behind an undo, as the sidebar row's button does.
pub(crate) fn discard_in(state: Store<UiModel>, environment: &EnvironmentId, draft_id: &str) {
    let Some(saved) = forget_draft(state, environment, draft_id) else { return };
    if state.destination().peek().as_ref() == Some(environment) && state.active_draft().peek().as_deref() == Some(draft_id) {
        state.active_draft().set(None);
    }
    let token = state.discarded_draft().peek().as_ref().map_or(0, |held| held.token) + 1;
    state.discarded_draft().set(Some(Discarded { token, saved }));
    dioxus::dioxus_core::spawn_forever(async move {
        runtime::delay(UNDO_MILLISECONDS).await;
        if state.discarded_draft().peek().as_ref().is_some_and(|held| held.token == token) {
            state.discarded_draft().set(None);
        }
    });
}

fn undo(state: Store<UiModel>) {
    let Some(held) = state.discarded_draft().write().take() else { return };
    state.peek().draft_storage.document.borrow_mut().restore(held.saved);
    crate::draft_storage::schedule(state);
    crate::draft_storage::restore_destination(state);
    bump(state);
}

/// The preview of a draft's row: the first line of its text, else what it holds besides.
fn preview(state: Store<UiModel>, target: &DraftTarget) -> String {
    let model = state.peek();
    let document = model.draft_storage.document.borrow();
    let text = document.prompt(target).unwrap_or_default();
    let line = text.trim().lines().next().unwrap_or_default().trim();
    if !line.is_empty() {
        return line.to_owned();
    }
    let count = crate::composer_chips::contexts_of(state, target).len()
        + match document.attachments(target) {
            t3_client::draft_storage::SavedAttachments::Edited(list) => list.len(),
            t3_client::draft_storage::SavedAttachments::Original(draft) => ["attachments", "files"].iter().map(|key| draft[*key].as_array().map_or(0, Vec::len)).sum(),
            t3_client::draft_storage::SavedAttachments::None => 0,
        };
    format!("{count} attachment{}", if count == 1 { "" } else { "s" })
}

/// The drafts to list: those of every environment this window knows that hold work, except the one the form is showing.
fn rows(state: Store<UiModel>) -> Vec<Row> {
    let showing = {
        let model = state.peek();
        (model.view == View::Chat && model.active_thread.is_none()).then(|| model.destination.clone().zip(model.active_draft.clone())).flatten()
    };
    let known: Vec<EnvironmentId> = state.environments().peek().records.keys().cloned().collect();
    let drafts = state.peek().draft_storage.document.borrow().drafts();
    drafts
        .into_iter()
        .filter_map(|session| Some((known.iter().find(|environment| environment.as_str() == session.environment)?.clone(), session)))
        .filter(|(environment, session)| showing.as_ref() != Some(&(environment.clone(), session.draft_id.clone())))
        .filter(|(_, session)| state.peek().draft_storage.document.borrow().has_content(&session.target()))
        .map(|(environment, session)| Row { preview: preview(state, &session.target()), environment, draft_id: session.draft_id, project_id: session.project_id })
        .collect()
}

/// The draft rows above the project groups. Memoised on what changes the list (a draft started, sent, discarded, or left for
/// another view), never on a keystroke in the open draft.
#[component]
pub(crate) fn Rows(state: Store<UiModel>, transport: runtime::TransportHandle) -> Element {
    let list = use_memo(move || {
        let _ = (state.draft_revision().read(), state.view().read(), state.active_thread().read(), state.active_draft().read(), state.destination().read(), state.environments().read());
        rows(state)
    });
    let list = list.read();
    // Nothing to draw and nothing read: the shell changes often, and a list that is empty should not follow it.
    if list.is_empty() {
        return rsx! {};
    }
    // Where a project's name is kept: the shell of its environment, the open one's or a parked one's.
    let projects: std::collections::BTreeMap<(EnvironmentId, String), String> = crate::fleet::read(state, |shells| shells.iter().flat_map(|shell| shell.snapshot.projects.iter().map(|project| ((shell.environment.clone(), project.id.to_string()), project.title.to_string()))).collect());
    let several = crate::fleet::count(state) > 1;
    rsx! {
        div { class: "draft-rows", role: "list", "aria-label": "Unsent drafts",
            for row in list.iter().cloned() {
                {
                    let name = projects.get(&(row.environment.clone(), row.project_id.clone())).cloned().unwrap_or_default();
                    let machine = if several { format!("{} \u{b7} ", crate::fleet::label(state, &row.environment)) } else { String::new() };
                    let (open, discard_id, id) = (row.draft_id.clone(), row.draft_id.clone(), format!("{}:{}", row.environment, row.draft_id));
                    let (environment, discard_environment) = (row.environment.clone(), row.environment.clone());
                    let transport = transport.clone();
                    rsx! {
                        div { class: "draft-row", key: "{id}", role: "listitem", "data-draft-id": "{row.draft_id}",
                            button { class: "draft-row-open", r#type: "button", "aria-label": "Unsent draft {row.preview}", title: "{machine}{name}",
                                onclick: move |_| { if crate::fleet::open(&transport, state, &environment) { runtime::open_draft(&transport, state, &open); } },
                                span { class: "draft-row-project", "\u{270e} {name}" }
                                span { class: "draft-row-preview", "{row.preview}" }
                            }
                            button { class: "draft-row-discard", r#type: "button", "aria-label": "Discard draft", title: "Discard draft", onclick: move |_| discard_in(state, &discard_environment, &discard_id), "\u{d7}" }
                        }
                    }
                }
            }
        }
    }
}

/// The notice that takes a discard back.
#[component]
pub(crate) fn UndoNotice(state: Store<UiModel>) -> Element {
    if state.discarded_draft().read().is_none() {
        return rsx! {};
    }
    rsx! {
        div { class: "undo-notice", role: "status",
            span { "Discarded draft" }
            button { r#type: "button", "aria-label": "Undo discard", onclick: move |_| undo(state), "Undo" }
        }
    }
}
