//! The thread details panel (`ThreadDetailsPanel.tsx`, `ThreadDetailsCard.tsx`, `ThreadAutomationsPanel.tsx`): a card at the top right of the
//! conversation that shows where the thread works and which scheduled tasks (automations) are bound to it, opened by the header's
//! toggle or by `threadPanel.toggle`. Here the card holds the Workspace summary and the Automations, with a switch, a manual run and
//! the way to the Scheduled tasks page for each; the controls the original also gathers in it (open in an editor, the branch, project
//! scripts, Git actions, the lineage) stay in the header, where this app has always drawn them. Whether the card is open is kept for each
//! thread while the page is open; it is not saved.
use crate::{
    notice::{self, Level},
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt, View},
    scheduled_tasks_page::{use_task_list, zone},
    settings_common::connected_allows,
};
use dioxus::prelude::*;
use std::collections::BTreeSet;
use t3_client::scheduled_tasks as tasks;
use t3_contracts::{AuthEnvironmentScope, ScheduledTaskRunStatus};

/// The key of a thread's card: environment and thread.
fn key_of(state: Store<UiModel>, thread_id: &str) -> Option<String> {
    Some(format!("{}\u{0}{thread_id}", state.destination().peek().as_ref()?))
}

/// Whether the card of this thread is open.
pub(crate) fn is_open(state: Store<UiModel>, thread_id: &str) -> bool {
    let Some(environment) = state.destination().read().as_ref().map(ToString::to_string) else { return false };
    state.thread_details().read().contains(&format!("{environment}\u{0}{thread_id}"))
}

/// `threadPanel.toggle`: opens the card of the open thread, or closes it.
pub(crate) fn toggle(state: Store<UiModel>) {
    let Some(thread_id) = state.active_thread().peek().clone() else { return };
    let Some(key) = key_of(state, &thread_id) else { return };
    let mut open = state.thread_details();
    let mut set = open.write();
    if !set.remove(&key) {
        set.insert(key);
    }
}

/// The header's button for the card (`Toggle thread details panel`).
#[component]
pub(crate) fn Toggle(state: Store<UiModel>, thread_id: String, hint: Option<String>) -> Element {
    let open = is_open(state, &thread_id);
    let title = format!("Toggle thread details{}", hint.map(|hint| format!(" ({hint})")).unwrap_or_default());
    rsx! { button { class: "icon-button", r#type: "button", "aria-label": "Toggle thread details panel", "aria-pressed": "{open}", title: "{title}", onclick: move |_| toggle(state), "\u{25a4} Details" } }
}

/// The card, when this thread's is open.
#[component]
pub(crate) fn Card(state: Store<UiModel>, transport: TransportHandle, thread_id: String) -> Element {
    if !is_open(state, &thread_id) {
        return rsx! {};
    }
    rsx! { aside { class: "thread-details-card", "aria-label": "Thread details", "data-thread-details-panel": "inline",
        Workspace { state, thread_id: thread_id.clone() }
        Automations { state, transport, thread_id }
    } }
}

/// Where the thread works: its project, the environment, the folder and the branch.
#[component]
fn Workspace(state: Store<UiModel>, thread_id: String) -> Element {
    let field = state.shell();
    let shell = field.read();
    let Some(snapshot) = shell.snapshot.as_ref() else { return rsx! {} };
    let Some(thread) = snapshot.threads.iter().find(|thread| thread.id.as_str() == thread_id) else { return rsx! {} };
    let project = snapshot.projects.iter().find(|project| project.id == thread.project_id);
    let environment = state.destination().read().as_ref().and_then(|id| state.environments().read().records.get(id).map(|record| record.label.clone()));
    let folder = thread.worktree_path.as_ref().map(ToString::to_string).or_else(|| project.map(|project| project.workspace_root.to_string()));
    let branch = thread.branch.as_ref().map(ToString::to_string);
    let worktree = thread.worktree_path.is_some();
    rsx! { section { class: "thread-details-section", "aria-label": "Workspace",
        dl {
            if let Some(project) = project { dt { "Project" } dd { "{project.title}" } }
            if let Some(environment) = environment { dt { "Environment" } dd { "{environment}" } }
            if let Some(folder) = folder { dt { if worktree { "Worktree" } else { "Folder" } } dd { class: "mono", "{folder}" } }
            if let Some(branch) = branch { dt { "Branch" } dd { class: "mono", "{branch}" } }
        }
    } }
}

/// The scheduled tasks bound to this thread, live. Nothing is drawn when there are none and the list loaded; a list that failed to
/// load is said, so missing controls do not read as "no automations" (`ThreadAutomationsPanel`).
#[component]
fn Automations(state: Store<UiModel>, transport: TransportHandle, thread_id: String) -> Element {
    let (list, readable) = use_task_list(state, &transport);
    let mut busy = use_signal(|| None::<String>);
    let operate = use_memo(move || connected_allows(state, AuthEnvironmentScope::OrchestrationOperate));
    let act = {
        let transport = transport.clone();
        use_callback(move |(id, method, payload, failure): (String, &'static str, serde_json::Value, &'static str)| {
            if busy.peek().is_some() || !*operate.peek() {
                return;
            }
            busy.set(Some(id));
            let transport = transport.clone();
            spawn(async move {
                let result = runtime::request_value_reported(transport, state, method, payload).await;
                busy.set(None);
                if let Err(message) = result {
                    notice::push(state, Level::Error, failure, message);
                }
            });
        })
    };
    let snapshot = list.read().clone();
    let bound: Vec<_> = snapshot.tasks.iter().flatten().filter(|task| task.thread_id.as_ref().is_some_and(|id| id.as_str() == thread_id)).cloned().collect();
    if snapshot.problem.is_none() && bound.is_empty() {
        return rsx! {};
    }
    let working = busy.read().clone();
    rsx! { section { class: "thread-details-section", "aria-label": "Automations", "data-thread-automations-panel": "true",
        header { class: "thread-details-heading",
            strong { "Automations" }
            button { class: "icon-button", r#type: "button", "aria-label": "Manage scheduled tasks", title: "Manage scheduled tasks", disabled: !readable(), onclick: move |_| state.view().set(View::ScheduledTasks), "\u{2699}" }
        }
        if let Some(problem) = snapshot.problem.clone() { p { class: "error-banner", role: "alert", "Could not load automations: {problem}" } }
        ul { class: "thread-automations",
            for task in bound {
                {
                    let summary = tasks::summary(&task, &zone);
                    let title = task.title.to_string();
                    let id = task.id.to_string();
                    let webhook = tasks::is_webhook(&task);
                    let dot = match task.last_run_status { ScheduledTaskRunStatus::Never => "never", ScheduledTaskRunStatus::Running => "running", ScheduledTaskRunStatus::Succeeded => "succeeded", ScheduledTaskRunStatus::Failed => "failed" };
                    let locked = working.is_some() || !operate();
                    let run_locked = locked || task.last_run_status == ScheduledTaskRunStatus::Running;
                    let (enable_id, run_id, enable_to) = (id.clone(), id.clone(), !task.enabled);
                    rsx! { li { key: "{id}", "data-task": "{id}",
                        span { class: "task-dot {dot}", role: "img", "aria-label": "Last run {dot}" }
                        div { class: "thread-automation-main",
                            strong { "{title}" }
                            p { class: "muted", "{summary.schedule} \u{b7} {summary.next}" }
                        }
                        button { class: "icon-button", r#type: "button", "aria-label": "Edit {title}", title: "Edit automation", onclick: move |_| state.view().set(View::ScheduledTasks), "Edit" }
                        // A webhook task runs from its address; there is no request to run it with.
                        if !webhook {
                            button { class: "icon-button", r#type: "button", "aria-label": "Run {title} now", title: "Run now", disabled: run_locked,
                                onclick: move |_| act.call((run_id.clone(), tasks::RUN_NOW, serde_json::json!({"id":run_id.clone()}), "Could not run automation")), "Run" }
                        }
                        label { class: "switch",
                            input { r#type: "checkbox", role: "switch", "aria-label": if task.enabled { format!("Pause {title}") } else { format!("Resume {title}") }, checked: task.enabled, disabled: locked,
                                onchange: move |_| act.call((enable_id.clone(), tasks::SET_ENABLED, serde_json::json!({"id":enable_id.clone(),"enabled":enable_to}), "Could not update automation")) }
                        }
                    } }
                }
            }
        }
    } }
}

/// The threads whose card is open, for a test to read.
#[cfg(test)]
pub(crate) fn open_keys(state: Store<UiModel>) -> BTreeSet<String> {
    state.thread_details().peek().clone()
}
