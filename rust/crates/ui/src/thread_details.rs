//! The thread details card (`ThreadDetailsPanel.tsx`, `ThreadDetailsCard.tsx`, `ThreadAutomationsPanel.tsx`): one card that gathers where the
//! thread works and the controls for it, opened by the header's toggle or by `threadPanel.toggle`. Where there is room beside the
//! conversation it is pinned inline to the top right and is open until closed; where there is not, it is a popover from the header
//! that starts closed. Its content folds by what fits (`t3_client::thread_panel`: full, compact, essential) and unfolds when
//! room returns. Whether each thread's card is open is saved with the right panel's tabs.
//!
//! The card hosts the header's own components, not copies: the editor picker, the branch and Git actions, the project's scripts and the
//! lineage. While the card shows, the header leaves those out (as the original's header has none of them); while it is closed the header
//! keeps them, which the original does not do: there, closed means not at hand.
//!
//! The conversation column is measured by a small script (`MEASURE`), since only the page knows its size and what the content needs.
//! The original also moves the chat lane left to make room for an inline card; this app's lane does not move, so the card goes inline only where
//! it fits beside the centred lane.
use crate::{
    notice::{self, Level},
    right_panel,
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt, View},
    scheduled_tasks_page::{use_task_list, zone},
    settings_common::connected_allows,
};
use dioxus::prelude::*;
use serde_json::Value;
use std::collections::BTreeMap;
use t3_client::{
    scheduled_tasks as tasks,
    thread_panel::{self as logic, Density, Heights, Placement, Presentation},
};
use t3_contracts::{AuthEnvironmentScope, ScheduledTaskRunStatus};

/// What the page has told of its size. `None` until it has: nothing is known of the room then, and the card is a popover.
#[derive(Debug, Clone, Default)]
pub struct Model {
    /// The conversation column's width and its height below the header.
    container: Option<(f64, f64)>,
    /// The header's height, where the card's top edge begins.
    top: f64,
    /// What the card's content needed at each density, by thread and presentation (`measurementKey`).
    measured: BTreeMap<String, Heights>,
}

/// The script that reports the sizes. It sends `{kind:"container",width,height,top}` when the column changes size and `{kind:"content",fitting}` when
/// the card's content does (the lineage scrolls as it grows, so it does not count toward what the card needs: counting it would hide the
/// other sections when the person asks to see more rows). It ends with the page's channel.
const MEASURE: &str = r#"
const workspace = document.querySelector('.workspace');
if (!workspace) { return; }
let content = null;
const contentObserver = new ResizeObserver(() => reportContent());
const reportContent = () => {
  if (!content) return;
  const frame = content.closest('[data-thread-details-card]');
  const next = content.offsetHeight + (frame ? frame.offsetHeight - frame.clientHeight : 0);
  const lineage = content.querySelector('[data-thread-relationships-panel]');
  dioxus.send({ kind: 'content', fitting: next - (lineage ? lineage.offsetHeight : 0) });
};
const reportContainer = () => {
  const header = workspace.querySelector(':scope > .workspace-header');
  const top = header ? header.offsetHeight : 0;
  const style = getComputedStyle(workspace);
  const width = workspace.clientWidth - parseFloat(style.paddingLeft) - parseFloat(style.paddingRight);
  dioxus.send({ kind: 'container', width, height: Math.max(0, workspace.clientHeight - top), top });
};
const bind = () => {
  const next = workspace.querySelector('[data-thread-details-content]');
  if (next === content) return;
  contentObserver.disconnect();
  content = next;
  if (content) { contentObserver.observe(content); reportContent(); }
};
const observer = new ResizeObserver(reportContainer);
observer.observe(workspace);
const mutations = new MutationObserver(bind);
mutations.observe(workspace, { childList: true, subtree: true });
bind();
reportContainer();
try { await dioxus.recv(); } finally { observer.disconnect(); mutations.disconnect(); contentObserver.disconnect(); }
"#;

/// The room beside the conversation for the card inline, when there is room (`resolveThreadDetailsCardLayout`).
fn placement(state: Store<UiModel>) -> Option<Placement> {
    let container = state.thread_card().read().container?;
    let placement = logic::layout(container, logic::LANE_PADDING, logic::MIN_CHAT_WIDTH, None, false)?;
    // The lane here does not move for the card, so it must not need to.
    (logic::lane_shift(container.0, placement.x) == 0.).then_some(placement)
}

/// How the card is presented now: inline where there is room, else a popover.
pub(crate) fn presentation(state: Store<UiModel>) -> Presentation {
    if placement(state).is_some() { Presentation::Inline } else { Presentation::Popover }
}

/// Whether the card of this thread is open in the presentation it has now.
pub(crate) fn is_open(state: Store<UiModel>, thread_id: &str) -> bool {
    right_panel::card_open(state, thread_id, presentation(state))
}

/// `threadPanel.toggle`: opens the card of the open thread, or closes it.
pub(crate) fn toggle(state: Store<UiModel>) {
    let Some(thread_id) = state.active_thread().peek().clone() else { return };
    right_panel::toggle_card(state, &thread_id, presentation(state));
}

/// The header's button for the card (`Toggle thread details panel`).
#[component]
pub(crate) fn Toggle(state: Store<UiModel>, thread_id: String, hint: Option<String>) -> Element {
    let open = is_open(state, &thread_id);
    let title = format!("Toggle thread details{}", hint.map(|hint| format!(" ({hint})")).unwrap_or_default());
    rsx! { button { class: "icon-button", r#type: "button", "aria-label": "Toggle thread details panel", "aria-pressed": "{open}", title: "{title}", onclick: move |_| toggle(state), "\u{25a4} Details" } }
}

/// Where a thread works, for the controls the card hosts: its project, the folder its controls act on, the project's own folder and its worktree.
struct Places {
    project_id: String,
    cwd: String,
    project_cwd: String,
    worktree: Option<String>,
}
fn workspace_of(state: Store<UiModel>, thread_id: &str) -> Option<Places> {
    let field = state.shell();
    let shell = field.read();
    let snapshot = shell.snapshot.as_ref()?;
    let thread = snapshot.threads.iter().find(|thread| thread.id.as_str() == thread_id)?;
    let project_cwd = snapshot.projects.iter().find(|project| project.id == thread.project_id).map(|project| project.workspace_root.to_string()).unwrap_or_default();
    let worktree = thread.worktree_path.as_ref().map(ToString::to_string);
    Some(Places { project_id: thread.project_id.to_string(), cwd: worktree.clone().unwrap_or_else(|| project_cwd.clone()), project_cwd, worktree })
}

/// Draws what the header has of the card's controls only while the card is not showing them. The check is read here, in a memo, so that the
/// header (and the application above it) does not repaint when the card's state or the page's size changes.
#[component]
pub(crate) fn HeaderOnly(state: Store<UiModel>, thread_id: String, children: Element) -> Element {
    let hosted = use_memo(use_reactive((&thread_id,), move |(thread_id,)| *state.view().read() == View::Chat && is_open(state, &thread_id)));
    if hosted() {
        return rsx! {};
    }
    rsx! { {children} }
}

/// The card of the open thread. Always mounted for the thread, so that the page's size is known whether it shows or not; it draws
/// when its thread's card is open.
#[component]
pub(crate) fn Card(state: Store<UiModel>, transport: TransportHandle, thread_id: String) -> Element {
    // The sizes the page reports.
    use_future(move || async move {
        let mut watcher = document::eval(MEASURE);
        while let Ok(message) = watcher.recv::<Value>().await {
            apply(state, &message);
        }
    });
    let placed = placement(state);
    let presentation = if placed.is_some() { Presentation::Inline } else { Presentation::Popover };
    // The popover closes when there is room to show the card inline (`ThreadDetailsCard`'s presentation effect).
    {
        let thread_id = thread_id.clone();
        use_effect(move || {
            if placement(state).is_some() && right_panel::card_open(state, &thread_id, Presentation::Popover) {
                right_panel::set_card_open(state, &thread_id, Presentation::Popover, false);
            }
        });
    }
    if !right_panel::card_open(state, &thread_id, presentation) {
        return rsx! {};
    }
    let card_field = state.thread_card();
    let (container, top) = { let card = card_field.read(); (card.container, card.top) };
    // The height the card has: its placement's, else what the column leaves below the header less the room the popover keeps.
    let height = placed.map_or_else(|| (container.map_or(0., |container| container.1) - 52.).max(0.), |placed| placed.height);
    let key = format!("{thread_id}:{presentation:?}");
    let heights = state.thread_card().read().measured.get(&key).copied().unwrap_or_default();
    let density = logic::density(height, heights);
    let density_name = match density { Density::Full => "full", Density::Compact => "compact", Density::Essential => "essential" };
    let parts = logic::parts(density, false);
    let mode = if placed.is_some() { "inline" } else { "popover" };
    let body = rsx! { div { "data-thread-details-content": "{key}", "data-density-key": "{density_name}",
        Workspace { state, transport: transport.clone(), thread_id: thread_id.clone(), parts }
        if parts.automations { Automations { state, transport: transport.clone(), thread_id: thread_id.clone() } }
        if parts.lineage { crate::thread_relations::Inline { state, transport: transport.clone(), thread_id: thread_id.clone() } }
    } };
    match placed {
        Some(placed) => rsx! {
            aside { class: "thread-details-card", "aria-label": "Thread details", "data-thread-details-panel": "{mode}", "data-thread-details-card": "true", "data-density": "{density_name}",
                style: "left:{placed.x}px;top:{top + placed.y}px;width:{placed.width}px;max-height:{placed.height}px;right:auto;", {body} }
        },
        None => rsx! {
            div { class: "thread-details-backdrop", onclick: { let thread_id = thread_id.clone(); move |_| right_panel::set_card_open(state, &thread_id, Presentation::Popover, false) } }
            aside { class: "thread-details-card thread-details-popover", "aria-label": "Thread details", "data-thread-details-panel": "{mode}", "data-thread-details-card": "true", "data-density": "{density_name}", tabindex: "-1",
                onmounted: move |event| { let element = event.data(); spawn(async move { let _ = element.set_focus(true).await; }); },
                onkeydown: { let thread_id = thread_id.clone(); move |event| { if event.key() == Key::Escape { event.stop_propagation(); right_panel::set_card_open(state, &thread_id, Presentation::Popover, false); } } },
                {body} }
        },
    }
}

/// What the page reported (`MEASURE`). A size that did not change writes nothing, so a steady page repaints nothing.
fn apply(state: Store<UiModel>, message: &Value) {
    match message["kind"].as_str() {
        Some("container") => {
            let (Some(width), Some(height), Some(top)) = (message["width"].as_f64(), message["height"].as_f64(), message["top"].as_f64()) else { return };
            let current = state.thread_card().peek().clone();
            if current.container != Some((width, height)) || current.top != top {
                let mut card = state.thread_card();
                let mut card = card.write();
                card.container = Some((width, height));
                card.top = top;
            }
        }
        Some("content") => {
            let Some(fitting) = message["fitting"].as_f64() else { return };
            // The card measured is the one on screen, at the density it is drawn.
            let Some(thread_id) = state.active_thread().peek().clone() else { return };
            let presentation = presentation(state);
            let key = format!("{thread_id}:{presentation:?}");
            let height = match presentation { Presentation::Inline => placement(state).map_or(0., |placed| placed.height), Presentation::Popover => (state.thread_card().peek().container.map_or(0., |container| container.1) - 52.).max(0.) };
            let current = state.thread_card().peek().measured.get(&key).copied().unwrap_or_default();
            let density = logic::density(height, current);
            let mut next = current;
            match density {
                Density::Full => next.full = fitting,
                Density::Compact => next.compact = fitting,
                Density::Essential => return,
            }
            if next != current {
                state.thread_card().write().measured.insert(key, next);
            }
        }
        _ => {}
    }
}

/// The card's first section: where the thread works (at full density the environment, folder and branch), then the controls it hosts. The
/// controls are the header's own components, each drawn by the density's parts.
#[component]
fn Workspace(state: Store<UiModel>, transport: TransportHandle, thread_id: String, parts: logic::Parts) -> Element {
    let Some(places) = workspace_of(state, &thread_id) else { return rsx! {} };
    let field = state.shell();
    let shell = field.read();
    let Some(snapshot) = shell.snapshot.as_ref() else { return rsx! {} };
    let Some(thread) = snapshot.threads.iter().find(|thread| thread.id.as_str() == thread_id) else { return rsx! {} };
    let project = snapshot.projects.iter().find(|project| project.id == thread.project_id);
    let environment = state.destination().read().as_ref().and_then(|id| state.environments().read().records.get(id).map(|record| record.label.clone()));
    let folder = thread.worktree_path.as_ref().map(ToString::to_string).or_else(|| project.map(|project| project.workspace_root.to_string()));
    let branch = thread.branch.as_ref().map(ToString::to_string);
    let worktree = thread.worktree_path.is_some();
    let persist_context_strip = state.client_settings().read().persist_composer_context_strip;
    rsx! {
        if parts.workspace_toolbar {
            section { class: "thread-details-section", "aria-label": "Workspace",
                dl {
                    if let Some(project) = project { dt { "Project" } dd { "{project.title}" } }
                    if let Some(environment) = environment { dt { "Environment" } dd { "{environment}" } }
                    if let Some(folder) = folder { dt { if worktree { "Worktree" } else { "Folder" } } dd { class: "mono", "{folder}" } }
                    if let Some(branch) = branch.clone() { dt { "Branch" } dd { class: "mono", "{branch}" } }
                }
            }
        }
        section { class: "thread-details-section thread-details-controls", "aria-label": "Controls", "data-separated": "{parts.separated}",
            if parts.open_in && !places.cwd.is_empty() { crate::open_in::OpenIn { key: "card-open-in:{thread_id}:{places.cwd}", state, transport: transport.clone(), cwd: places.cwd.clone() } }
            crate::project_actions::RunControl { key: "card-run:{thread_id}", state, thread_id: thread_id.clone() }
            if !places.cwd.is_empty() && !persist_context_strip { crate::branch::Control { key: "card-branch:{thread_id}:{places.cwd}", state, transport: transport.clone(), thread_id: thread_id.clone(), project_id: places.project_id.clone(), cwd: places.cwd.clone(), project_cwd: places.project_cwd.clone(), worktree: places.worktree.clone() } }
        }
    }
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

/// The threads whose card is open in the presentation it has now and is not at its default, for a test to read.
#[cfg(test)]
pub(crate) fn open_keys(state: Store<UiModel>) -> Vec<String> {
    let presentation = presentation(state);
    let panels = state.right_panel().peek().panels.clone();
    panels.visibilities.keys().into_iter().filter(|key| panels.visibilities.is_open(key, presentation)).collect()
}

/// Tells the card the page's size, as the script would.
#[cfg(test)]
pub(crate) fn set_container(state: Store<UiModel>, width: f64, height: f64) {
    apply(state, &serde_json::json!({"kind":"container","width":width,"height":height,"top":48.}));
}
/// Tells the card what its content needed at each density, as the script would.
#[cfg(test)]
pub(crate) fn set_measured(state: Store<UiModel>, thread_id: &str, presentation: Presentation, heights: Heights) {
    state.thread_card().write().measured.insert(format!("{thread_id}:{presentation:?}"), heights);
}
