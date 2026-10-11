//! Ending a thread or a project: archive (with the undo the original offers for five seconds),
//! unarchive, delete a thread, remove a project. Port of the sidebar and `useThreadActions`
//! flows; the questions are asked in the original's words (`t3_client::lifecycle`) and nothing is
//! easier to trigger than there. Every write is guarded against a second click until the server
//! answers, and a refusal shows the server's own reason.
//!
//! Deleting a thread stops its runs and removes its attachments and terminals on the server; it
//! leaves the worktree, its branch and the checkpoint refs on disk, and says nothing more than
//! the original does.
use crate::{
    notice::{self, Level},
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
};
use dioxus::prelude::*;
use serde_json::{Value, json};
use t3_client::{connection::ConnectionStatus, lifecycle as words};
use t3_contracts::{AuthEnvironmentScope, ThreadShell};

#[derive(Debug, Clone, PartialEq)]
pub struct Project {
    pub id: String,
    pub title: String,
    pub path: String,
    pub threads: usize,
}
#[derive(Debug, Clone, PartialEq)]
pub enum Dialog {
    Archive { id: String, title: String },
    Delete { id: String, title: String },
    /// The chosen threads of the sidebar.
    DeleteMany { ids: Vec<String> },
    Unpin { id: String, title: String },
    /// The first answer to removing a project that has threads.
    NotEmpty(Project),
    Remove(Project),
}
/// The archives the reader can still take back, for a few seconds.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Undo {
    pub token: u64,
    pub ids: Vec<String>,
    pub back_to: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Model {
    pub dialog: Option<Dialog>,
    pub busy: bool,
    pub undo: Option<Undo>,
    pub tokens: u64,
    /// Moves when a thread goes to or comes back from the archive, so an open list reads again.
    pub archive_revision: u64,
}

/// How long an archive can be taken back, as in the original.
const UNDO_MILLISECONDS: u64 = 5_000;

fn can_operate(state: Store<UiModel>) -> bool {
    *state.status().peek() == ConnectionStatus::Connected && state.destination().peek().as_ref().is_some_and(|destination| state.environments().peek().allows(destination, AuthEnvironmentScope::OrchestrationOperate))
}
fn thread_of(state: Store<UiModel>, id: &str) -> Option<ThreadShell> {
    state.shell().peek().snapshot.as_ref().and_then(|snapshot| snapshot.threads.iter().find(|thread| thread.id.as_str() == id).cloned())
}
/// Whether the thread's provider is at work, which keeps it from being archived.
pub(crate) fn archive_blocked(thread: &ThreadShell) -> bool {
    !words::can_archive(thread.status, thread.active_run_id.is_some())
}

async fn dispatch(transport: &TransportHandle, state: Store<UiModel>, kind: &str, mut payload: Value) -> Result<Value, String> {
    payload["type"] = json!(kind);
    payload["commandId"] = json!(uuid::Uuid::new_v4().to_string());
    runtime::request_value_reported(transport.clone(), state, "orchestration.dispatchCommand", payload).await
}

/// Runs one write: nothing starts while another is in flight; the answer, unless the connection
/// it was sent on has gone, is handed to `finish`, and the guard is released either way.
fn run<F, Fut>(state: Store<UiModel>, transport: &TransportHandle, work: F)
where
    F: FnOnce(TransportHandle) -> Fut + 'static,
    Fut: std::future::Future<Output = ()> + 'static,
{
    if state.lifecycle().peek().busy || !can_operate(state) {
        return;
    }
    state.lifecycle().write().busy = true;
    let transport = transport.clone();
    dioxus::dioxus_core::spawn_forever(async move {
        work(transport).await;
        state.lifecycle().write().busy = false;
    });
}

fn leave_if_open(state: Store<UiModel>, transport: &TransportHandle, thread: &ThreadShell) {
    if state.active_thread().peek().as_deref() == Some(thread.id.as_str()) {
        runtime::new_thread(transport, state, Some(thread.project_id.to_string()));
    }
}

/// Archive asks first only when the reader's settings say so.
pub(crate) fn request_archive(state: Store<UiModel>, transport: &TransportHandle, id: &str) {
    let Some(thread) = thread_of(state, id).filter(|thread| !archive_blocked(thread)) else { return };
    if state.client_settings().peek().confirm_thread_archive {
        state.lifecycle().write().dialog = Some(Dialog::Archive { id: id.into(), title: thread.title.to_string() });
    } else {
        archive(state, transport, id);
    }
}
fn archive(state: Store<UiModel>, transport: &TransportHandle, id: &str) {
    let Some(thread) = thread_of(state, id).filter(|thread| !archive_blocked(thread)) else { return };
    let id = id.to_owned();
    run(state, transport, move |transport| async move {
        let owner = runtime::response_owner(&transport, state);
        let answer = dispatch(&transport, state, "thread.archive", json!({"threadId":id})).await;
        if owner.is_none() || owner != runtime::response_owner(&transport, state) {
            return;
        }
        match answer {
            Ok(_) => {
                state.lifecycle().write().archive_revision += 1;
                let back_to = state.active_thread().peek().clone().filter(|open| *open == id);
                leave_if_open(state, &transport, &thread);
                let token = state.lifecycle().peek().tokens + 1;
                {
                    let mut field = state.lifecycle();
                    let mut model = field.write();
                    model.tokens = token;
                    // Consecutive archives share one notice and are taken back together.
                    let mut undo = model.undo.take().unwrap_or_default();
                    undo.token = token;
                    undo.ids.push(id);
                    undo.back_to = back_to.or(undo.back_to);
                    model.undo = Some(undo);
                }
                // The notice expires on its own: one wait, never a poll, and not while the guard
                // that keeps a second archive out is still held.
                dioxus::dioxus_core::spawn_forever(async move {
                    runtime::delay(UNDO_MILLISECONDS).await;
                    if state.lifecycle().peek().undo.as_ref().is_some_and(|undo| undo.token == token) {
                        state.lifecycle().write().undo = None;
                    }
                });
            }
            Err(reason) => notice::push(state, Level::Error, "Failed to archive thread", reason),
        }
    });
}

/// Takes the latest archives back. Consumed before anything is sent, so a repeated click or key
/// cannot restore the same group twice.
pub(crate) fn undo(state: Store<UiModel>, transport: &TransportHandle) {
    let Some(undo) = state.lifecycle().write().undo.take() else { return };
    let transport = transport.clone();
    for id in undo.ids.iter() {
        unarchive_inner(state, &transport, id.clone(), undo.back_to.clone().filter(|back| back == id));
    }
}
pub(crate) fn unarchive(state: Store<UiModel>, transport: &TransportHandle, id: &str) {
    unarchive_inner(state, transport, id.to_owned(), None);
}
fn unarchive_inner(state: Store<UiModel>, transport: &TransportHandle, id: String, reopen: Option<String>) {
    if !can_operate(state) {
        return;
    }
    let transport = transport.clone();
    dioxus::dioxus_core::spawn_forever(async move {
        let owner = runtime::response_owner(&transport, state);
        let answer = dispatch(&transport, state, "thread.unarchive", json!({"threadId":id})).await;
        if owner.is_none() || owner != runtime::response_owner(&transport, state) {
            return;
        }
        match answer {
            Ok(_) => {
                state.lifecycle().write().archive_revision += 1;
                if let Some(back) = reopen {
                    runtime::select_thread(&transport, state, back);
                }
            }
            Err(reason) => notice::push(state, Level::Error, "Failed to undo archive", reason),
        }
    });
}

/// Deleting a thread always asks, unless the reader turned the question off.
pub(crate) fn request_delete(state: Store<UiModel>, transport: &TransportHandle, id: &str) {
    let Some(thread) = thread_of(state, id) else { return };
    if state.client_settings().peek().confirm_thread_delete {
        state.lifecycle().write().dialog = Some(Dialog::Delete { id: id.into(), title: thread.title.to_string() });
    } else {
        delete(state, transport, id);
    }
}
/// Unpinning asks first only when the reader's settings say so (`confirmThreadUnpin`).
pub(crate) fn request_unpin(state: Store<UiModel>, transport: &TransportHandle, id: &str) {
    let Some(thread) = thread_of(state, id) else { return };
    if state.client_settings().peek().confirm_thread_unpin {
        state.lifecycle().write().dialog = Some(Dialog::Unpin { id: id.into(), title: thread.title.to_string() });
    } else {
        runtime::command(transport, state, "thread.unpin", json!({"threadId":id}));
    }
}

/// "Restart agent session": stops the provider session of the open thread, which keeps the conversation. The next message
/// starts a fresh process that resumes it and reloads skills, plugins and MCP servers; the provider's catalog is read again
/// for the thread's workspace so the composer's slash menu follows.
pub(crate) fn restart_session(state: Store<UiModel>, transport: &TransportHandle) {
    let Some(id) = state.active_thread().peek().clone() else { return };
    let Some(thread) = thread_of(state, &id) else { return };
    let sessions: Vec<String> = state.thread().peek().projection.as_ref().and_then(|projection| projection["providerSessions"].as_array()).map(|sessions| sessions.iter().filter_map(|session| session["id"].as_str().map(str::to_owned)).collect()).unwrap_or_default();
    let cwd = thread.worktree_path.as_ref().map(ToString::to_string).or_else(|| state.shell().peek().snapshot.as_ref().and_then(|snapshot| snapshot.projects.iter().find(|project| project.id == thread.project_id).map(|project| project.workspace_root.to_string())));
    let instance = if crate::sidebar_rows::runtime_instance(&thread) { thread.provider_instance_id.to_string() } else { thread.model_selection.instance_id.to_string() };
    let transport = transport.clone();
    dioxus::dioxus_core::spawn_forever(async move {
        let owner = runtime::response_owner(&transport, state);
        for session in sessions {
            if let Err(reason) = dispatch(&transport, state, "provider-session.detach", json!({"threadId":id,"providerSessionId":session,"reason":"client-requested"})).await {
                notice::push(state, Level::Error, "Could not restart the agent session", reason);
                return;
            }
        }
        notice::push(state, Level::Success, "Agent session will restart", "Your next message starts a fresh session.");
        let Some(cwd) = cwd else { return };
        if owner.is_none() || owner != runtime::response_owner(&transport, state) {
            return;
        }
        if let Err(reason) = runtime::request_value_reported(transport.clone(), state, "server.refreshProviders", json!({"instanceId":instance,"cwd":cwd,"fresh":true})).await {
            notice::push(state, Level::Error, "Could not refresh the provider", reason);
        }
    });
}

/// Deleting the chosen threads asks first (once, for all of them) unless the reader turned the question off.
pub(crate) fn request_delete_many(state: Store<UiModel>, transport: &TransportHandle, ids: Vec<String>) {
    if ids.is_empty() {
        return;
    }
    if state.client_settings().peek().confirm_thread_delete {
        state.lifecycle().write().dialog = Some(Dialog::DeleteMany { ids });
    } else {
        delete_many(state, transport, ids);
    }
}
/// One delete at a time, in the order chosen; those that went leave the choice, a failure is said once and
/// leaves the rest of the choice as it was (`deleteSelectedThreadEntries`).
fn delete_many(state: Store<UiModel>, transport: &TransportHandle, ids: Vec<String>) {
    let selected = ids.clone();
    let looking_at = state.active_thread().peek().clone().and_then(|open| thread_of(state, &open));
    run(state, transport, move |transport| async move {
        let owner = runtime::response_owner(&transport, state);
        let mut deleted = std::collections::BTreeSet::new();
        let mut failure = None;
        for id in &ids {
            let answer = dispatch(&transport, state, "thread.delete", json!({"threadId":id})).await;
            if owner.is_none() || owner != runtime::response_owner(&transport, state) {
                return;
            }
            match answer {
                Ok(_) => {
                    deleted.insert(id.clone());
                }
                Err(reason) => {
                    failure = Some(reason);
                    break;
                }
            }
        }
        if let Some(reason) = failure {
            notice::push(state, Level::Error, "Failed to delete threads", reason);
        }
        if !deleted.is_empty() {
            state.lifecycle().write().archive_revision += 1;
        }
        let leave = looking_at.filter(|thread| deleted.contains(thread.id.as_str()));
        let remove = t3_client::thread_selection::deselect_after_delete(&selected, &deleted, |id| thread_of(state, id).is_some_and(|thread| thread.deleted_at.is_none()));
        state.selection().write().remove(&remove);
        // The thread being looked at is gone: the view goes to a new thread of its project.
        if let Some(thread) = leave {
            leave_if_open(state, &transport, &thread);
        }
    });
}
fn delete(state: Store<UiModel>, transport: &TransportHandle, id: &str) {
    let Some(thread) = thread_of(state, id) else { return };
    // The view goes to the project's next thread, chosen before the delete as the original does (`getFallbackThreadIdAfterDelete`).
    let order = state.client_settings().peek().sidebar_thread_sort_order;
    let fallback = state.shell().peek().snapshot.as_ref().and_then(|snapshot| t3_client::thread_sort::fallback_after_delete(&snapshot.threads, id, order).map(|next| next.id.to_string()));
    let id = id.to_owned();
    run(state, transport, move |transport| async move {
        let owner = runtime::response_owner(&transport, state);
        let answer = dispatch(&transport, state, "thread.delete", json!({"threadId":id})).await;
        if owner.is_none() || owner != runtime::response_owner(&transport, state) {
            return;
        }
        match answer {
            Ok(_) => {
                state.lifecycle().write().archive_revision += 1;
                match fallback {
                    Some(next) if state.active_thread().peek().as_deref() == Some(thread.id.as_str()) => runtime::select_thread(&transport, state, next),
                    _ => leave_if_open(state, &transport, &thread),
                }
            }
            Err(reason) => notice::push(state, Level::Error, "Failed to delete thread", reason),
        }
    });
}

fn project_of(state: Store<UiModel>, id: &str) -> Option<Project> {
    let model = state.shell();
    let shell = model.peek();
    let snapshot = shell.snapshot.as_ref()?;
    let project = snapshot.projects.iter().find(|project| project.id.as_str() == id)?;
    let threads = snapshot.threads.iter().filter(|thread| thread.project_id == project.id && thread.deleted_at.is_none()).count();
    Some(Project { id: id.into(), title: project.title.to_string(), path: project.workspace_root.to_string(), threads })
}
/// A project with threads is refused first, with a way to go on; an empty one is asked about at once.
pub(crate) fn request_remove_project(state: Store<UiModel>, id: &str) {
    let Some(project) = project_of(state, id) else { return };
    state.lifecycle().write().dialog = Some(if project.threads > 0 { Dialog::NotEmpty(project) } else { Dialog::Remove(project) });
}
fn remove_project(state: Store<UiModel>, transport: &TransportHandle, id: &str) {
    let id = id.to_owned();
    let open_in_it = state.active_thread().peek().clone().and_then(|open| thread_of(state, &open)).is_some_and(|thread| thread.project_id.as_str() == id);
    run(state, transport, move |transport| async move {
        let owner = runtime::response_owner(&transport, state);
        let payload = json!({"type":"project.delete","commandId":uuid::Uuid::new_v4().to_string(),"projectId":id,"force":true});
        let answer = runtime::request_value_reported(transport.clone(), state, "projects.mutate", payload).await.map_err(|message| message);
        if owner.is_none() || owner != runtime::response_owner(&transport, state) {
            return;
        }
        match answer {
            Ok(_) => {
                if open_in_it {
                    runtime::new_thread(&transport, state, None);
                }
                if state.selected_project().peek().as_deref() == Some(id.as_str()) {
                    state.selected_project().set(None);
                }
            }
            Err(reason) => notice::push(state, Level::Error, "Failed to remove project", reason),
        }
    });
}

/// The header's archive and delete buttons (and unarchive, on an archived thread).
#[component]
pub(crate) fn HeaderButtons(state: Store<UiModel>, transport: TransportHandle, thread_id: String) -> Element {
    let Some(thread) = state.shell().read().snapshot.as_ref().and_then(|snapshot| snapshot.threads.iter().find(|thread| thread.id.as_str() == thread_id).cloned()) else { return rsx! {} };
    let allowed = *state.status().read() == ConnectionStatus::Connected && state.destination().read().as_ref().is_some_and(|destination| state.environments().read().allows(destination, AuthEnvironmentScope::OrchestrationOperate));
    let idle = !state.lifecycle().read().busy;
    let archived = thread.archived_at.is_some();
    let blocked = archive_blocked(&thread);
    rsx! {
        if archived {
            button { class: "icon-button", "aria-label": "Unarchive thread", title: "Unarchive thread", disabled: !allowed, onclick: { let (transport, id) = (transport.clone(), thread_id.clone()); move |_| unarchive(state, &transport, &id) }, "Unarchive" }
        } else {
            button { class: "icon-button", "aria-label": "Archive thread", title: if blocked { "A thread cannot be archived while it is working." } else { "Archive thread" }, disabled: !allowed || !idle || blocked,
                onclick: { let (transport, id) = (transport.clone(), thread_id.clone()); move |_| request_archive(state, &transport, &id) }, "Archive" }
        }
        button { class: "icon-button", "aria-label": "Delete thread", title: "Delete thread", disabled: !allowed || !idle, onclick: { let (transport, id) = (transport.clone(), thread_id.clone()); move |_| request_delete(state, &transport, &id) }, "Delete" }
    }
}

/// The question being asked, and the notice that takes an archive back.
#[component]
pub(crate) fn Host(state: Store<UiModel>, transport: TransportHandle) -> Element {
    let model = state.lifecycle().read().clone();
    let environments = state.environments().read().records.len();
    let environment_label = state.destination().read().as_ref().and_then(|destination| state.environments().read().records.get(destination).map(|record| record.label.clone())).filter(|_| environments > 1);
    let cancel = move |_| state.lifecycle().write().dialog = None;
    rsx! {
        if let Some(undo) = model.undo.clone() {
            div { class: "undo-notice", role: "status",
                span { "{words::undo_line(\"Archived\", undo.ids.len())}" }
                button { r#type: "button", "aria-label": "Undo archive", onclick: { let transport = transport.clone(); move |_| undo_archive(state, &transport) }, "Undo" }
            }
        }
        if let Some(dialog) = model.dialog.clone() {
            {
                let (message, ask_again) = match &dialog {
                    Dialog::Archive { title, .. } => (words::archive_thread_message(title), false),
                    Dialog::Delete { title, .. } => (words::delete_thread_message(title), false),
                    Dialog::DeleteMany { ids } => (t3_client::thread_selection::delete_message(ids.len()), false),
                    Dialog::Unpin { title, .. } => (format!("Unpin thread \"{title}\"?\nThis will move the thread out of your pinned section."), false),
                    Dialog::NotEmpty(_) => (String::new(), true),
                    Dialog::Remove(project) => (words::remove_project_message(&project.title, &project.path, environment_label.as_deref(), project.threads), false),
                };
                let (title, description) = if ask_again { (words::NOT_EMPTY_TITLE.to_owned(), Some(words::NOT_EMPTY_BODY.to_owned())) } else { words::confirm_copy(&message) };
                let destructive = !matches!(dialog, Dialog::Archive { .. } | Dialog::Unpin { .. });
                let confirm_label = if ask_again { words::NOT_EMPTY_ACTION } else { words::CONFIRM_BUTTON };
                let busy = model.busy;
                let transport = transport.clone();
                rsx! {
                    div { class: "modal-backdrop", onclick: cancel,
                        div { class: "dialog confirm-dialog", role: "alertdialog", "aria-modal": "true", "aria-labelledby": "lifecycle-heading", "aria-describedby": "lifecycle-body", tabindex: "-1",
                            onclick: move |event| event.stop_propagation(),
                            onmounted: move |event| { let element = event.data(); spawn(async move { let _ = element.set_focus(true).await; }); },
                            onkeydown: move |event| { if event.key() == Key::Escape { event.stop_propagation(); state.lifecycle().write().dialog = None; } },
                            h2 { id: "lifecycle-heading", "{title}" }
                            if let Some(description) = description { p { id: "lifecycle-body", style: "white-space: pre-line", "{description}" } }
                            div { class: "dialog-actions",
                                button { r#type: "button", "aria-label": "Cancel", onclick: cancel, "Cancel" }
                                button { class: if destructive { "destructive" } else { "primary" }, r#type: "button", "aria-label": "{confirm_label}", disabled: busy, onclick: move |_| {
                                    state.lifecycle().write().dialog = None;
                                    match dialog.clone() {
                                        Dialog::Archive { id, .. } => archive(state, &transport, &id),
                                        Dialog::Delete { id, .. } => delete(state, &transport, &id),
                                        Dialog::DeleteMany { ids } => delete_many(state, &transport, ids),
                                        Dialog::Unpin { id, .. } => { runtime::command(&transport, state, "thread.unpin", json!({"threadId":id})); }
                                        Dialog::NotEmpty(project) => {
                                            // Going on asks the long question, with the count as it is now.
                                            let latest = project_of(state, &project.id).unwrap_or(project);
                                            state.lifecycle().write().dialog = Some(Dialog::Remove(latest));
                                        }
                                        Dialog::Remove(project) => remove_project(state, &transport, &project.id),
                                    }
                                }, "{confirm_label}" }
                            }
                        }
                    }
                }
            }
        }
    }
}
fn undo_archive(state: Store<UiModel>, transport: &TransportHandle) {
    undo(state, transport);
}

/// A project's archived threads, read when the list is opened and again when one comes or goes.
/// The active shell does not carry them; the server keeps them in a shell of their own.
#[component]
pub(crate) fn Archived(state: Store<UiModel>, transport: TransportHandle, project_id: String) -> Element {
    let mut open = use_signal(|| false);
    let mut list = use_signal(|| None::<Result<Vec<(String, String)>, String>>);
    let revision = state.lifecycle().read().archive_revision;
    let readable = use_memo(move || *state.status().read() == ConnectionStatus::Connected && state.destination().read().as_ref().is_some_and(|destination| state.environments().read().allows(destination, AuthEnvironmentScope::OrchestrationRead)));
    let allowed = *state.status().read() == ConnectionStatus::Connected && state.destination().read().as_ref().is_some_and(|destination| state.environments().read().allows(destination, AuthEnvironmentScope::OrchestrationOperate));
    {
        let (transport, project_id) = (transport.clone(), project_id.clone());
        use_resource(move || {
            let (transport, project_id, wanted, _revision) = (transport.clone(), project_id.clone(), open() && readable(), revision);
            async move {
                if !wanted {
                    return;
                }
                let owner = runtime::response_owner(&transport, state);
                let answer = runtime::request_value_reported(transport.clone(), state, "orchestration.getArchivedShellSnapshot", json!({})).await;
                if owner.is_none() || owner != runtime::response_owner(&transport, state) {
                    return;
                }
                list.set(Some(answer.map(|snapshot| {
                    snapshot["threads"].as_array().into_iter().flatten().filter(|thread| thread["projectId"] == project_id.as_str() && thread["deletedAt"].is_null()).filter_map(|thread| Some((thread["id"].as_str()?.to_owned(), thread["title"].as_str()?.to_owned()))).collect()
                })));
            }
        });
    }
    rsx! {
        div { class: "archived-threads",
            button { r#type: "button", "aria-label": "Show archived threads", "aria-expanded": "{open()}", disabled: !readable(), onclick: move |_| open.set(!open()),
                if open() { "Hide archived" } else { "Archived" } }
            if open() {
                match list.read().clone() {
                    None => rsx! { p { class: "muted", role: "status", "Loading archived threads\u{2026}" } },
                    Some(Err(message)) => rsx! { p { class: "error-banner", role: "alert", "{message}" } },
                    Some(Ok(threads)) if threads.is_empty() => rsx! { p { class: "muted", "Nothing archived." } },
                    Some(Ok(threads)) => rsx! {
                        for (id, title) in threads {
                            div { key: "{id}", class: "thread-line",
                                oncontextmenu: { let (transport, id) = (transport.clone(), id.clone()); move |event| { event.prevent_default(); let at = event.client_coordinates(); let (transport, id) = (transport.clone(), id.clone()); spawn(async move { archived_menu(state, transport, id, allowed, (at.x, at.y)).await }); } },
                                button { class: "thread-row", onclick: { let (transport, id) = (transport.clone(), id.clone()); move |_| runtime::select_thread(&transport, state, id.clone()) }, span { class: "thread-title", "{title}" } }
                                button { "aria-label": "Unarchive {title}", disabled: !allowed, onclick: { let (transport, id) = (transport.clone(), id.clone()); move |_| unarchive(state, &transport, &id) }, "Unarchive" }
                                button { "aria-label": "Delete {title}", disabled: !allowed, onclick: { let (transport, id) = (transport.clone(), id.clone()); move |_| request_delete(state, &transport, &id) }, "Delete" }
                            }
                        }
                    },
                }
            }
        }
    }
}

/// The menu of an archived thread: bring it back, or delete it (which asks first).
async fn archived_menu(state: Store<UiModel>, transport: TransportHandle, id: String, allowed: bool, at: (f64, f64)) {
    let items: Vec<_> = t3_client::context_menu::archived_thread().into_iter().map(|item| { let disabled = !allowed; item.disabled(disabled) }).collect();
    match crate::context_menu::show("Archived thread actions", items, Some(at)).await.as_deref() {
        Some("unarchive") => unarchive(state, &transport, &id),
        Some("delete") => request_delete(state, &transport, &id),
        _ => {}
    }
}
