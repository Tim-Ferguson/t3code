//! "Implement in a new thread" on a plan: a thread in the same project and workspace whose first
//! turn implements the plan, started with the plan as its source. Port of
//! `onImplementPlanInNewThread` (`ChatView.tsx`): two commands (`thread.create`, then
//! `message.dispatch` with `sourcePlanRef`), the second sent only once the first is answered; a
//! failed start deletes the thread it made, and a started one is opened.
//!
//! This is not `runtime::launch_thread`, which starts a thread from the new-thread form's draft and
//! whose payload (`orchestration.launchThread`) has no field to name a source plan.
use crate::{
    notice::{self, Level},
    runtime::{self, UiModel, UiModelStoreExt},
    settings_common::connected_allows,
};
use dioxus::prelude::*;
use serde_json::{Value, json};
use t3_client::{new_thread, thread_title, timeline_items as words};
use t3_contracts::AuthEnvironmentScope;

/// What starting the thread sends, from the state as it is now; none when it cannot start.
struct Start {
    project_id: String,
    title: String,
    selection: t3_contracts::ModelSelection,
    mode: t3_contracts::RuntimeMode,
    branch: Option<String>,
    worktree: Option<String>,
}
fn start_of(state: Store<UiModel>, thread_id: &str, plan: &words::ProposedPlan) -> Option<Start> {
    let model = state.peek();
    let destination = model.destination.clone()?;
    let snapshot = model.shell.snapshot.as_ref()?;
    let thread = snapshot.threads.iter().find(|thread| thread.id.as_str() == thread_id)?;
    let project = snapshot.projects.iter().find(|project| project.id == thread.project_id)?;
    let config = model.typed_config.as_ref()?;
    // The model the composer has chosen for this thread; the new thread starts with the
    // project's default permissions, as the original's does.
    let choices = model.thread_choices.get(&(destination.clone(), thread_id.to_owned())).cloned().unwrap_or_default();
    let selection = choices.model_selection.unwrap_or_else(|| thread.model_selection.clone());
    let mode = new_thread::project_defaults(&config.settings, Some(project)).runtime_mode;
    let (selection, mode) = new_thread::launch_selection(config, &model.client_settings, &selection, mode)?;
    Some(Start {
        project_id: thread.project_id.to_string(),
        title: thread_title::truncate(&words::plan_thread_title(&plan.markdown), 50),
        selection,
        mode,
        branch: thread.branch.as_ref().map(ToString::to_string),
        worktree: thread.worktree_path.as_ref().map(ToString::to_string),
    })
}

async fn dispatch(transport: &runtime::TransportHandle, state: Store<UiModel>, kind: &str, mut payload: Value) -> Result<Value, String> {
    payload["type"] = json!(kind);
    payload["commandId"] = json!(uuid::Uuid::new_v4().to_string());
    runtime::request_value_reported(transport.clone(), state, "orchestration.dispatchCommand", payload).await
}

/// Starts the implementation thread, once. The new thread is opened when its first turn has been
/// accepted; a failure puts nothing behind it and says so.
pub(crate) fn implement_in_new_thread(transport: &runtime::TransportHandle, state: Store<UiModel>, thread_id: &str, plan: &words::ProposedPlan) {
    if state.plan_launch().peek().is_some() || !connected_allows(state, AuthEnvironmentScope::OrchestrationOperate) {
        return;
    }
    let Some(start) = start_of(state, thread_id, plan) else {
        notice::push(state, Level::Error, "Could not start implementation thread", "Selected provider is unavailable on this environment.");
        return;
    };
    *state.plan_launch().write() = Some(plan.id.clone());
    let (transport, source_thread, plan) = (transport.clone(), thread_id.to_owned(), plan.clone());
    // Not tied to the menu that started it: accepting the plan removes that menu.
    dioxus::dioxus_core::spawn_forever(async move {
        let owner = runtime::response_owner(&transport, state);
        let new_thread_id = uuid::Uuid::new_v4().to_string();
        let created = dispatch(&transport, state, "thread.create", json!({
            "createdBy":"user","creationSource":runtime::creation_source(),"threadId":new_thread_id,"projectId":start.project_id,"title":start.title,
            "modelSelection":start.selection,"runtimeMode":start.mode,"interactionMode":"default","branch":start.branch,"worktreePath":start.worktree,
        })).await;
        let mut failure = created.err();
        if failure.is_none() {
            let started = dispatch(&transport, state, "message.dispatch", json!({
                "createdBy":"user","creationSource":runtime::creation_source(),"threadId":new_thread_id,"messageId":uuid::Uuid::new_v4().to_string(),
                "text":words::plan_implementation_prompt(&plan.markdown),"attachments":[],"titleSeed":start.title,"modelSelection":start.selection,
                "sourcePlanRef":{"threadId":source_thread,"planId":plan.id},
                "dispatchMode":{"type":"start_immediately"},"deliveryIntent":"auto",
            })).await;
            failure = started.err();
            if failure.is_some() {
                // Nothing was started: the thread made for it goes too.
                let _ = dispatch(&transport, state, "thread.delete", json!({"threadId":new_thread_id})).await;
            }
        }
        *state.plan_launch().write() = None;
        // An answer for another server, or after a reconnect, is not this one's to act on.
        let now = runtime::response_owner(&transport, state);
        if !owner.as_ref().zip(now.as_ref()).is_some_and(|(before, after)| before.same_connection(after)) {
            return;
        }
        match failure {
            Some(reason) => notice::push(state, Level::Error, "Could not start implementation thread", reason),
            None => runtime::select_thread(&transport, state, new_thread_id),
        }
    });
}

/// The small menu beside "Implement": its one entry starts the plan in a new thread. Escape closes it.
#[component]
pub(crate) fn ImplementActions(state: Store<UiModel>, transport: runtime::TransportHandle, thread_id: String, plan: words::ProposedPlan, disabled: bool) -> Element {
    let mut open = use_signal(|| false);
    let starting = state.plan_launch().read().is_some();
    rsx! {
        span { class: "implement-actions",
            button { class: "icon-button", r#type: "button", "aria-label": "Implementation actions", "aria-haspopup": "menu", "aria-expanded": "{open()}", disabled: disabled || starting,
                onclick: move |_| open.set(!open()), "\u{25be}" }
            if open() {
                div { class: "menu-backdrop", onclick: move |_| open.set(false) }
                div { class: "context-menu", role: "menu", "aria-label": "Implementation menu", tabindex: "0",
                    style: "bottom:100%;right:0;position:absolute",
                    onmounted: move |event| { drop(event.set_focus(true)); },
                    onkeydown: move |event| { if event.key() == Key::Escape { event.stop_propagation(); open.set(false); } },
                    button { class: "menu-item", role: "menuitem", r#type: "button", "aria-label": "Implement in a new thread", disabled: disabled || starting,
                        onclick: move |_| { open.set(false); implement_in_new_thread(&transport, state, &thread_id, &plan); },
                        "Implement in a new thread" }
                }
            }
        }
    }
}
