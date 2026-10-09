//! Tracked repository clones: the environment's clone stream, the notice that follows a clone
//! wherever the person is, the banner over a project's new-thread form, and the marker on the
//! project in the sidebar. Port of `ProjectCloneToastCoordinator.tsx` and the project-clone
//! banner of `ChatView.tsx`.
//!
//! The server creates the project at once, pointing at the empty destination, and clones in the
//! background. The stream carries its progress: one subscription for the whole app while the
//! environment tracks clones, replaced by nothing when the connection is lost (the last list
//! stays and every action is off until it returns). A project whose clone has not landed does
//! not start threads. Cancel, Retry and Remove each go out once and stay off until the server
//! has answered.
use crate::{
    provider_manage::{allowed_now, ask},
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
};
use dioxus::prelude::*;
use futures_util::StreamExt;
use serde_json::json;
use std::collections::BTreeSet;
use t3_client::{connection::ConnectionStatus, project_clone as logic};
use t3_contracts::AuthEnvironmentScope;

/// What the stream has said, and which clone actions are waiting on the server.
#[derive(Debug, Clone, Default)]
pub(crate) struct Model {
    pub list: Vec<logic::Snapshot>,
    /// Whether the list is the stream's current word. After a lost connection it is not.
    pub live: bool,
    pub busy: BTreeSet<String>,
}

fn supported(state: Store<UiModel>) -> bool {
    state.typed_config().read().as_ref().is_some_and(|config| config.environment.capabilities.project_clone_tracking == Some(true))
}

/// The one subscription of the app: held while connected to an environment that tracks clones.
pub(crate) fn use_stream(state: Store<UiModel>, transport: TransportHandle) {
    use_resource(move || {
        let transport = transport.clone();
        async move {
            let tracking = supported(state);
            let connected = *state.status().read() == ConnectionStatus::Connected;
            let readable = state.destination().read().as_ref().is_some_and(|id| state.environments().read().allows(id, AuthEnvironmentScope::OrchestrationRead));
            let mut model = state.clones();
            if !(tracking && connected && readable) {
                if model.peek().live {
                    model.write().live = false;
                }
                return;
            }
            let mut wait = 500;
            loop {
                if *state.status().peek() != ConnectionStatus::Connected {
                    return;
                }
                if let Ok(mut stream) = runtime::request_stream(&transport, state, logic::STREAM, json!({})) {
                    while let Some(Ok(values)) = stream.next().await {
                        let list = logic::decode_list(&values);
                        let mut model = state.clones();
                        if model.peek().list != list || !model.peek().live {
                            let mut write = model.write();
                            write.list = list;
                            write.live = true;
                        }
                        wait = 500;
                    }
                }
                if model.peek().live {
                    model.write().live = false;
                }
                runtime::delay(wait).await;
                wait = (wait * 2).min(5000);
            }
        }
    });
}

/// The tracked clone of a project, read by whoever shows it, so that only they repaint.
pub(crate) fn use_clone_of(state: Store<UiModel>, project_id: String) -> Memo<Option<logic::Snapshot>> {
    use_memo(use_reactive((&project_id,), move |(project_id,)| state.clones().read().list.iter().find(|snapshot| snapshot.project_id == project_id).cloned()))
}

/// Why a thread cannot start in this project, when its clone has not landed.
pub(crate) fn block_reason(state: Store<UiModel>, project_id: &str) -> Option<&'static str> {
    logic::send_block_reason(state.clones().read().list.iter().find(|snapshot| snapshot.project_id == project_id))
}

fn writable(state: Store<UiModel>) -> bool {
    allowed_now(state, AuthEnvironmentScope::SourceControlWrite) && state.clones().peek().live
}

/// Cancels or retries a clone. The control stays off until the server has answered; a request
/// that did not take effect says so.
pub(crate) fn act(transport: &TransportHandle, state: Store<UiModel>, method: &'static str, project_id: String, failure: &'static str) {
    if state.clones().peek().busy.contains(&project_id) || !writable(state) {
        return;
    }
    let mut model = state.clones();
    model.write().busy.insert(project_id.clone());
    let transport = transport.clone();
    spawn(async move {
        let answer = ask(&transport, state, method, json!({"projectId":project_id})).await;
        let mut model = state.clones();
        model.write().busy.remove(&project_id);
        match answer {
            Some(Err(message)) => crate::notice::push(state, crate::notice::Level::Error, failure, message),
            Some(Ok(value)) if value["applied"] == false => crate::notice::push(state, crate::notice::Level::Warning, failure, "The clone had already changed."),
            _ => {}
        }
    });
}

/// Which clone a notice would repeat for the project whose form is open: the banner there shows
/// the same progress, so the notice steps aside.
fn viewing_draft(state: Store<UiModel>, project_id: &str) -> bool {
    state.active_thread().read().is_none() && state.selected_project().read().as_deref() == Some(project_id)
}

/// One notice per clone: its progress with Cancel while it runs, then its outcome.
#[component]
pub(crate) fn Notices(state: Store<UiModel>, transport: TransportHandle) -> Element {
    let model = state.clones().read().clone();
    let can_act = writable(state);
    if model.list.is_empty() {
        return rsx! {};
    }
    rsx! { div { class: "notices clone-notices", role: "region", "aria-label": "Repository clones",
        for clone in model.list.iter().filter(|clone| !viewing_draft(state, &clone.project_id)) {
            {
                let name = logic::display_name(clone);
                let busy = model.busy.contains(&clone.project_id);
                let id = clone.project_id.clone();
                let (class, title, detail) = match clone.phase.as_str() {
                    "running" => ("notice", format!("Cloning {name}"), logic::progress_summary(clone)),
                    "done" => ("notice success", format!("Cloned {name}"), clone.destination_path.clone()),
                    "cancelled" => ("notice", format!("Cancelled cloning {name}"), clone.destination_path.clone()),
                    _ => ("notice failure", format!("Failed to clone {name}"), clone.error.clone().unwrap_or_else(|| "The clone failed.".into())),
                };
                rsx! { div { key: "{clone.project_id}", class: "{class}", role: if clone.phase == "failed" { "alert" } else { "status" },
                    div { class: "notice-text", strong { "{title}" } span { "{detail}" } }
                    match clone.phase.as_str() {
                        "running" => rsx! { button { class: "notice-follow", r#type: "button", "aria-label": "Cancel cloning {name}", disabled: busy || !can_act, onclick: { let transport = transport.clone(); let id = id.clone(); move |_| act(&transport, state, logic::CANCEL, id.clone(), "Failed to cancel clone") }, "Cancel" } },
                        "done" => rsx! { button { class: "notice-follow", r#type: "button", "aria-label": "Open project {name}", onclick: { let transport = transport.clone(); let id = id.clone(); move |_| runtime::new_thread(&transport, state, Some(id.clone())) }, "Open project" } },
                        _ => rsx! {
                            button { class: "notice-follow", r#type: "button", "aria-label": "Retry cloning {name}", disabled: busy || !can_act, onclick: { let transport = transport.clone(); let id = id.clone(); move |_| act(&transport, state, logic::RETRY, id.clone(), "Failed to retry clone") }, "Retry" }
                            button { class: "notice-follow", r#type: "button", "aria-label": "Remove project {name}", onclick: { let id = id.clone(); move |_| crate::thread_lifecycle::request_remove_project(state, &id) }, "Remove project" }
                        },
                    }
                } }
            }
        }
    } }
}

/// The banner over a project's new-thread form while its clone has not landed.
#[component]
pub(crate) fn Banner(state: Store<UiModel>, transport: TransportHandle, project_id: String) -> Element {
    let snapshot = use_clone_of(state, project_id.clone());
    let busy = state.clones().read().busy.contains(&project_id);
    let can_act = writable(state);
    let Some(clone) = snapshot.read().clone().filter(|clone| clone.phase != "done") else { return rsx! {} };
    let name = logic::display_name(&clone);
    let cancelled = clone.phase == "cancelled";
    let heading = format!("{}{name}", if cancelled { "Cancelled cloning " } else { "Failed to clone " });
    let reason = if cancelled { "Retry to bring in the repository.".to_owned() } else { clone.error.clone().unwrap_or_default() };
    rsx! { section { class: "composer-panel clone-banner", role: if clone.phase == "running" { "status" } else { "alert" }, "aria-label": "Repository clone",
        match clone.phase.as_str() {
            "running" => rsx! {
                div { class: "usage-limits-head", strong { "Cloning {name}" } span { class: "muted", "{logic::progress_summary(&clone)}" } }
                button { r#type: "button", "aria-label": "Cancel clone", disabled: busy || !can_act, onclick: { let (transport, id) = (transport.clone(), project_id.clone()); move |_| act(&transport, state, logic::CANCEL, id.clone(), "Failed to cancel clone") }, "Cancel" }
            },
            _ => rsx! {
                div { class: "usage-limits-head", strong { "{heading}" } span { class: "muted", "{reason}" } }
                button { r#type: "button", "aria-label": "Remove project", onclick: { let id = project_id.clone(); move |_| crate::thread_lifecycle::request_remove_project(state, &id) }, "Remove project" }
                button { r#type: "button", "aria-label": "Retry clone", disabled: busy || !can_act, onclick: { let (transport, id) = (transport.clone(), project_id.clone()); move |_| act(&transport, state, logic::RETRY, id.clone(), "Failed to retry clone") }, "Retry" }
            },
        }
    } }
}

/// The cloning state of a project, under its heading in the sidebar. It reads only its own
/// clone, so progress repaints this line and no other row.
#[component]
pub(crate) fn ProjectState(state: Store<UiModel>, project_id: String) -> Element {
    let snapshot = use_clone_of(state, project_id);
    let Some(clone) = snapshot.read().clone().filter(|clone| clone.phase != "done") else { return rsx! {} };
    let text = match clone.phase.as_str() {
        "running" => format!("Cloning \u{b7} {}", logic::progress_summary(&clone)),
        "cancelled" => "Clone cancelled".to_owned(),
        _ => "Clone failed".to_owned(),
    };
    rsx! { span { class: "project-clone-state muted", role: "status", title: "{text}", "{text}" } }
}
