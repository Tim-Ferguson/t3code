//! Settings, Scheduled tasks: the environment's tasks as a live list, a form to create or edit
//! one (on an interval, at a time of day, or when a webhook is called), pause, run now, delete,
//! and a webhook's address, token and delivery log. Port of `ScheduledTasksSettings.tsx`, for the
//! one environment this app is connected to.
//!
//! The list stream is held only while the page is mounted and every piece of state here is a
//! signal of these components, so a task changing on the server repaints this page and nothing
//! else. After a lost connection the last list stays on screen, marked out of date, and nothing
//! can be changed from it. Times are absolute: a "next run in 4m" label would need a timer.
use crate::{
    notice,
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt, View},
    settings_common::{Confirm, Tabs, connected_allows, copy_text},
};
use dioxus::prelude::*;
use serde_json::json;
use std::collections::BTreeSet;
use t3_client::{
    connection::ConnectionStatus,
    new_thread,
    scheduled_tasks::{self as logic, Draft, ScheduleMode, WorkspaceMode},
};
use t3_contracts::{AuthEnvironmentScope, ModelSelection, ScheduledTask, ScheduledTaskWebhookDelivery, ScheduledTaskWebhookDeliverySummary};

/// The viewer's offset from UTC, in minutes east, at an instant.
pub(crate) fn zone(milliseconds: i64) -> i32 {
    #[cfg(target_arch = "wasm32")]
    {
        -(js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(milliseconds as f64)).get_timezone_offset() as i32)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        use chrono::{Offset, TimeZone};
        chrono::Local.timestamp_millis_opt(milliseconds).single().map_or(0, |at| at.offset().fix().local_minus_utc() / 60)
    }
}

/// What the stream has told the page. `live` is false whenever the list may be out of date:
/// before the first snapshot, while reconnecting, and after the connection is lost.
#[derive(Debug, Clone, PartialEq, Default)]
struct List {
    tasks: Option<Vec<ScheduledTask>>,
    live: bool,
    problem: Option<String>,
}

#[component]
pub(crate) fn Page(state: Store<UiModel>, transport: TransportHandle) -> Element {
    let destination = state.destination().read().clone();
    let label = destination.as_ref().and_then(|id| state.environments().read().records.get(id).map(|record| record.label.clone())).unwrap_or_default();
    rsx! { section { class: "settings-page scheduled-tasks-page",
        Tabs { state, current: View::ScheduledTasks }
        h1 { "Scheduled tasks" }
        if let Some(destination) = destination {
            Tasks { key: "{destination}", state, transport, environment_label: label }
        } else {
            p { "Connect to an environment to manage its scheduled tasks." }
            button { onclick: move |_| state.view().set(View::Connections), "Open connections" }
        }
    } }
}

fn editor_key(target: &Option<ScheduledTask>) -> String {
    target.as_ref().map_or_else(|| "new".to_owned(), |task| task.id.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Action {
    Enable(bool),
    Run,
    Delete,
}

#[component]
fn Tasks(state: Store<UiModel>, transport: TransportHandle, environment_label: String) -> Element {
    let mut list = use_signal(List::default);
    let mut editor = use_signal(|| None::<Option<ScheduledTask>>);
    let mut deliveries = use_signal(|| None::<ScheduledTask>);
    let mut deleting = use_signal(|| None::<(String, String)>);
    let mut busy = use_signal(BTreeSet::<String>::new);
    let readable = use_memo(move || connected_allows(state, AuthEnvironmentScope::OrchestrationRead));
    // Whether the session may change tasks at all decides which controls exist; whether it can
    // right now (connected, and not looking at a list that may be out of date) decides whether
    // they work.
    let operator = use_memo(move || state.destination().read().as_ref().is_some_and(|destination| state.environments().read().allows(destination, AuthEnvironmentScope::OrchestrationOperate)));
    let writable = use_memo(move || operator() && connected_allows(state, AuthEnvironmentScope::OrchestrationOperate) && list.read().live);

    // The list stream: one subscription while this page is on screen and the connection can read
    // it. The resource restarts, and drops the stream, when the connection changes.
    {
        let transport = transport.clone();
        use_resource(move || {
            let (transport, readable) = (transport.clone(), readable());
            async move {
                if list.peek().live {
                    list.write().live = false;
                }
                if !readable {
                    return;
                }
                let mut wait = 500;
                loop {
                    // Without a connection there is nothing to ask; the resource starts over when
                    // one returns, instead of this loop failing against a closed socket.
                    if *state.status().peek() != ConnectionStatus::Connected {
                        return;
                    }
                    match runtime::request_stream(&transport, state, logic::SUBSCRIBE, json!({})) {
                        Err(error) => list.write().problem = Some(error),
                        Ok(mut stream) => {
                            while let Some(result) = stream.next().await {
                                let values = match result {
                                    Ok(values) => values,
                                    Err(error) => {
                                        list.write().problem = Some(error);
                                        break;
                                    }
                                };
                                for value in values {
                                    match logic::decode_tasks(&value) {
                                        Ok(tasks) => {
                                            wait = 500;
                                            let next = List { tasks: Some(tasks), live: true, problem: None };
                                            if *list.peek() != next {
                                                *list.write() = next;
                                            }
                                        }
                                        Err(error) => list.write().problem = Some(error),
                                    }
                                }
                            }
                            if list.peek().live {
                                list.write().live = false;
                            }
                        }
                    }
                    runtime::delay(wait).await;
                    wait = (wait * 2).min(5000);
                }
            }
        });
    }

    // Every row action says what went wrong where the user is looking, and runs alone per task.
    let act = {
        let transport = transport.clone();
        use_callback(move |(id, action): (String, Action)| {
            if busy.peek().contains(&id) || !*writable.peek() {
                return;
            }
            let Some(owner) = runtime::connection_owner(&transport, state) else { return };
            busy.write().insert(id.clone());
            let transport = transport.clone();
            spawn(async move {
                let (method, payload) = match action {
                    Action::Enable(enabled) => (logic::SET_ENABLED, json!({"id":id,"enabled":enabled})),
                    Action::Run => (logic::RUN_NOW, json!({"id":id})),
                    Action::Delete => (logic::DELETE, json!({"id":id})),
                };
                let result = runtime::request_value_reported(transport.clone(), state, method, payload).await;
                busy.write().remove(&id);
                if runtime::connection_owner(&transport, state).as_ref() != Some(&owner) {
                    return;
                }
                if let Err(message) = result {
                    notice::push(state, notice::Level::Error, "Could not update scheduled task", message);
                }
            });
        })
    };

    let snapshot = list.read().clone();
    let can_operate = operator();
    let can_write = writable();
    rsx! {
        p { class: "muted", "Tasks of {environment_label}. {logic::TIME_ZONE_NOTE} Times shown are in your time zone." }
        if !readable() { p { class: "muted", role: "status", "Connect to {environment_label} to see its scheduled tasks." } }
        else if !can_operate { p { class: "muted", role: "status", "This connection can view scheduled tasks but cannot change them." } }
        if snapshot.tasks.is_some() && !snapshot.live { p { class: "stale-banner", role: "status", "This list may be out of date. It is shown as last received and cannot be changed until the connection returns." } }
        if let Some(problem) = snapshot.problem.clone() { p { class: "error-banner", role: "alert", "Could not load scheduled tasks. {problem}" } }
        if can_operate { div { class: "keybinding-toolbar",
            button { class: "primary", "aria-label": "New task", disabled: !can_write || snapshot.tasks.is_none(), onclick: move |_| editor.set(Some(None)), "New task" }
        } }
        match snapshot.tasks.clone() {
            None => rsx! { if snapshot.problem.is_none() && readable() { p { class: "muted", role: "status", "Loading scheduled tasks\u{2026}" } } },
            Some(tasks) if tasks.is_empty() => rsx! { crate::empty_state::EmptyState { title: "No scheduled tasks", description: "Create one to run a prompt on a schedule or when a webhook is called.", } },
            Some(tasks) => rsx! { ul { class: "task-list", "aria-label": "Scheduled tasks",
                for task in tasks {
                    TaskRow { key: "{task.id}", task: task.clone(), busy: busy.read().contains(task.id.as_str()), operate: can_operate, writable: can_write,
                        onenable: { let id = task.id.to_string(); move |on| act.call((id.clone(), Action::Enable(on))) },
                        onrun: { let id = task.id.to_string(); move |_| act.call((id.clone(), Action::Run)) },
                        onedit: { let task = task.clone(); move |_| editor.set(Some(Some(task.clone()))) },
                        ondeliveries: { let task = task.clone(); move |_| deliveries.set(Some(task.clone())) },
                        ondelete: { let (id, title) = (task.id.to_string(), task.title.to_string()); move |_| deleting.set(Some((id.clone(), title.clone()))) } }
                }
            } },
        }
        if let Some(target) = editor.read().clone() {
            Editor { key: "{editor_key(&target)}", state, transport: transport.clone(), list, target, writable: can_write, environment_label: environment_label.clone(), onclose: move |_| editor.set(None) }
        }
        if let Some(task) = deliveries.read().clone() {
            Deliveries { key: "{task.id}", state, transport: transport.clone(), task, onclose: move |_| deliveries.set(None) }
        }
        if let Some((id, title)) = deleting.read().clone() {
            Confirm { label: "delete", title: format!("Delete \u{201c}{title}\u{201d}?"), body: "The task stops running and is removed. This cannot be undone.", confirm: "Delete task", busy: !can_write,
                oncancel: move |_| deleting.set(None),
                onconfirm: move |_| { deleting.set(None); act.call((id.clone(), Action::Delete)); } }
        }
    }
}

#[component]
fn TaskRow(task: ScheduledTask, busy: bool, operate: bool, writable: bool, onenable: EventHandler<bool>, onrun: EventHandler<()>, onedit: EventHandler<()>, ondeliveries: EventHandler<()>, ondelete: EventHandler<()>) -> Element {
    let summary = logic::summary(&task, &zone);
    let webhook = logic::is_webhook(&task);
    let title = task.title.to_string();
    rsx! { li { class: "task-row", "data-task": "{task.id}",
        div { class: "task-main",
            strong { "{title}" }
            p { class: "task-prompt muted", "{task.prompt}" }
            p { class: "task-status",
                span { "{summary.schedule} \u{b7} {summary.next}" }
                if let Some(last) = summary.last.clone() { span { class: "muted", "{last}" } }
                if let Some(status) = summary.status { span { class: logic::status_tone(status).class(), "{status.as_str()}" } }
            }
            if let Some(error) = summary.error.clone() { p { class: "task-error", role: "alert", "aria-label": "Last run error of {title}", "{error}" } }
        }
        if operate {
            div { class: "task-actions",
                label { class: "switch",
                    input { r#type: "checkbox", role: "switch", "aria-label": "Enable {title}", checked: task.enabled, disabled: busy || !writable, onchange: move |event| onenable.call(event.checked()) }
                    if task.enabled { "Enabled" } else { "Paused" }
                }
                button { "aria-label": "Edit {title}", disabled: busy || !writable, onclick: move |_| onedit.call(()), "Edit" }
                if webhook { button { "aria-label": "Deliveries of {title}", disabled: busy, onclick: move |_| ondeliveries.call(()), "Deliveries" } }
                else { button { "aria-label": "Run {title} now", disabled: busy || !writable, onclick: move |_| onrun.call(()), "Run now" } }
                button { "aria-label": "Delete {title}", disabled: busy || !writable, onclick: move |_| ondelete.call(()), "Delete" }
            }
        }
    } }
}

// ---- the form ----

/// The projects the shell lists, as (id, title).
fn projects(state: Store<UiModel>) -> Vec<(String, String)> {
    state.shell().read().snapshot.as_ref().map(|snapshot| snapshot.projects.iter().map(|project| (project.id.to_string(), project.title.to_string())).collect()).unwrap_or_default()
}
fn key(selection: &ModelSelection) -> String {
    serde_json::to_string(&(&selection.instance_id, &selection.model)).expect("model identity")
}

/// The model rows the picker lists and the selection it shows: the draft's own choice, else the
/// project's, the server's and the first usable model, as a new thread resolves them.
fn picker(state: Store<UiModel>, draft: &Draft) -> (Vec<new_thread::InstanceModelOption>, Option<ModelSelection>) {
    let config_field = state.typed_config();
    let config = config_field.read();
    let Some(config) = config.as_ref() else { return (vec![], None) };
    let client_field = state.client_settings();
    let client = client_field.read();
    let project = state.shell().read().snapshot.as_ref().and_then(|snapshot| snapshot.projects.iter().find(|project| project.id.as_str() == draft.project_id).cloned());
    let defaults = new_thread::project_defaults(&config.settings, project.as_ref());
    let explicit = logic::split_model_key(&draft.model_key).and_then(|(instance, model)| match &draft.base_model_selection {
        Some(base) if base.instance_id.as_str() == instance && base.model.as_str() == model => Some(base.clone()),
        _ => Some(ModelSelection { instance_id: instance.parse().ok()?, model: t3_contracts::TrimmedNonEmptyString::new(model).ok()?, options: None }),
    });
    let sticky = state.destination().peek().as_ref().and_then(|id| state.sticky_models().peek().get(id).cloned());
    let selected = new_thread::resolve_new_thread_selection(config, &client, runtime::ui_surface(), explicit.as_ref(), &defaults, sticky.as_ref());
    (new_thread::available_models(config, &client, runtime::ui_surface(), selected.as_ref()), selected)
}

#[component]
fn Editor(state: Store<UiModel>, transport: TransportHandle, list: Signal<List>, target: Option<ScheduledTask>, writable: bool, environment_label: String, onclose: EventHandler<()>) -> Element {
    let first_project = projects(state).first().map(|(id, _)| id.clone()).unwrap_or_default();
    let mut draft = use_signal(|| target.as_ref().map(Draft::from_task).unwrap_or_else(|| Draft::new(&first_project)));
    let mut saving = use_signal(|| false);
    let mut problem = use_signal(|| None::<String>);
    let editing = target.as_ref().map(|task| task.id.to_string());
    // The live row, so a rotated URL shows up without reopening the form; once the list has
    // loaded, a task that is gone is gone.
    let live_task = match (&editing, list.read().tasks.as_ref()) {
        (Some(id), Some(tasks)) => tasks.iter().find(|task| task.id.as_str() == id).cloned(),
        (_, _) => target.clone(),
    };
    let missing = editing.is_some() && live_task.is_none();
    let projects = projects(state);
    let current = draft.read().clone();
    let (rows, selected) = picker(state, &current);
    let selected_key = selected.as_ref().map(key).unwrap_or_default();
    let preserved_missing = selected.as_ref().is_some_and(|selected| !rows.iter().any(|row| row.instance_id == selected.instance_id && row.model.slug == selected.model.as_str()));
    let can_save = writable && !*saving.read() && !missing && list.read().tasks.is_some();
    let save = {
        let (transport, project_ids, selection) = (transport.clone(), projects.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(), selected.clone());
        let saved_secret = live_task.as_ref().and_then(|task| task.webhook.as_ref()).is_some_and(|webhook| webhook.has_secret);
        move |_| {
            if !can_save {
                return;
            }
            let context = logic::Context { project_ids: &project_ids, selection: selection.clone(), has_saved_secret: saved_secret };
            let input = match draft.peek().upsert_input(&context, runtime::creation_source()) {
                Ok(input) => input,
                Err(invalid) => {
                    problem.set(Some(format!("{}. {}", invalid.title, invalid.message)));
                    return;
                }
            };
            let Some(owner) = runtime::connection_owner(&transport, state) else { return };
            saving.set(true);
            problem.set(None);
            let transport = transport.clone();
            spawn(async move {
                let result = runtime::request_value_reported(transport.clone(), state, logic::UPSERT, input).await;
                saving.set(false);
                if runtime::connection_owner(&transport, state).as_ref() != Some(&owner) {
                    return;
                }
                match result {
                    Ok(_) => onclose.call(()),
                    Err(message) => problem.set(Some(format!("Could not save scheduled task. {message}"))),
                }
            });
        }
    };
    let title = if current.editing_id.is_some() { "Edit task" } else { "New task" };
    let mode = current.schedule_mode;
    rsx! { div { class: "modal-backdrop",
        div { class: "dialog wide-dialog task-editor", role: "dialog", "aria-modal": "true", "aria-label": "Task editor", tabindex: "-1",
            onmounted: move |event| { let element = event.data(); spawn(async move { let _ = element.set_focus(true).await; }); },
            onkeydown: move |event| { if event.key() == Key::Escape && !*saving.peek() { event.stop_propagation(); onclose.call(()); } },
            h2 { "{title}" }
            p { class: "muted", "Run a prompt automatically \u{2014} on an interval, at a fixed time, or when a webhook is called. Runs on {environment_label}." }
            fieldset { class: "task-form", disabled: *saving.read(),
                if !writable { p { class: "form-problem", role: "status", "Reconnect this environment before saving." } }
                if missing { p { class: "form-problem", role: "status", "This scheduled task no longer exists." } }
                label { r#for: "task-title", "Name" }
                input { id: "task-title", "aria-label": "Name", placeholder: "e.g. Check for Sentry issues", value: "{current.title}", oninput: move |event| draft.write().title = event.value() }
                div { class: "field-row",
                    div { label { r#for: "task-project", "Project" }
                        select { id: "task-project", "aria-label": "Project", value: "{current.project_id}", onchange: move |event| draft.write().project_id = event.value(),
                            if current.project_id.is_empty() || !projects.iter().any(|(id, _)| *id == current.project_id) { option { value: "{current.project_id}", selected: true, "Select a project" } }
                            for (id, name) in projects.iter() { option { key: "{id}", value: "{id}", selected: *id == current.project_id, "{name}" } }
                        } }
                    div { label { r#for: "task-workspace", "Workspace" }
                        select { id: "task-workspace", "aria-label": "Workspace", value: "{current.workspace_mode.value()}", onchange: move |event| if let Some(mode) = WorkspaceMode::parse(&event.value()) { draft.write().workspace_mode = mode },
                            for mode in [WorkspaceMode::Worktree, WorkspaceMode::Root, WorkspaceMode::ExistingWorktree] { option { key: "{mode.value()}", value: "{mode.value()}", selected: mode == current.workspace_mode, "{mode.label()}" } }
                        } }
                }
                if current.workspace_mode == WorkspaceMode::Worktree {
                    label { r#for: "task-base-ref", "Base branch" }
                    input { id: "task-base-ref", "aria-label": "Base branch", value: "{current.base_ref}", oninput: move |event| draft.write().base_ref = event.value() }
                    label { class: "check", input { r#type: "checkbox", "aria-label": "Start from the remote branch", checked: current.start_from_origin, onchange: move |event| draft.write().start_from_origin = event.checked() } "Start from the remote branch" }
                }
                if current.workspace_mode == WorkspaceMode::ExistingWorktree {
                    label { r#for: "task-checkout", "Checkout path" }
                    input { id: "task-checkout", "aria-label": "Checkout path", placeholder: "/path/to/checkout", value: "{current.existing_worktree_path}", oninput: move |event| draft.write().existing_worktree_path = event.value() }
                }
                label { r#for: "task-prompt", "Prompt" }
                textarea { id: "task-prompt", "aria-label": "Prompt", placeholder: "What should the agent do each time this runs?", value: "{current.prompt}", oninput: move |event| draft.write().prompt = event.value() }
                label { r#for: "task-model", "Model" }
                select { id: "task-model", "aria-label": "Model", value: "{selected_key}", onchange: move |event| {
                        if let Ok((instance, model)) = serde_json::from_str::<(String, String)>(&event.value()) { draft.write().model_key = format!("{instance}:{model}"); }
                    },
                    if preserved_missing { if let Some(selected) = selected.as_ref() { option { value: "{key(selected)}", selected: true, "{selected.instance_id} \u{b7} {selected.model}" } } }
                    if rows.is_empty() && selected.is_none() { option { value: "", selected: true, "No models available" } }
                    for row in rows.iter() { option { key: "{key(&row.selection())}", value: "{key(&row.selection())}", selected: key(&row.selection()) == selected_key, "{row.provider_label} \u{b7} {row.model.name}" } }
                }
                if let Some(task) = target.as_ref().filter(|task| matches!(&task.schedule, t3_contracts::ScheduledTaskSchedule::Interval(fields) if fields.every_ms.0 < t3_contracts::MIN_SCHEDULED_TASK_INTERVAL_MS)) {
                    p { class: "muted", role: "status", "This task uses a legacy interval below one minute. Saving updates it to at least one minute. ({logic::schedule_label(&task.schedule)})" }
                }
                div { class: "schedule-mode", role: "group", "aria-label": "Schedule type",
                    span { "Schedule" }
                    for (value, label) in [(ScheduleMode::Fixed, "At a time"), (ScheduleMode::Interval, "Every interval"), (ScheduleMode::Webhook, "On webhook")] {
                        button { key: "{label}", r#type: "button", class: if mode == value { "selected" } else { "" }, "aria-pressed": "{mode == value}", "aria-label": "{label}", onclick: move |_| draft.write().set_mode(value), "{label}" }
                    }
                }
                match mode {
                    ScheduleMode::Webhook => rsx! {
                        WebhookEndpoint { state, transport: transport.clone(), task: live_task.clone(), can_rotate: writable }
                        p { class: "muted", {logic::WEBHOOK_HELP} }
                        label { r#for: "task-max-age", "Skip requests older than (minutes, optional)" }
                        input { id: "task-max-age", r#type: "number", "aria-label": "Skip requests older than", min: "1", max: "{t3_contracts::MAX_WEBHOOK_DELIVERY_AGE_MINUTES}", placeholder: "Run every request", value: "{current.max_delivery_age_minutes}", oninput: move |event| draft.write().max_delivery_age_minutes = event.value() }
                        label { class: "check", input { r#type: "checkbox", "aria-label": "Require signature", checked: current.signature_enabled, onchange: move |event| draft.write().signature_enabled = event.checked() } "Require signature" }
                        p { class: "muted", "Reject requests without a valid HMAC-SHA256 signature of the body." }
                        if current.signature_enabled {
                            div { class: "field-row",
                                div { label { r#for: "task-sig-header", "Header" } input { id: "task-sig-header", "aria-label": "Header", value: "{current.signature_header}", oninput: move |event| draft.write().signature_header = event.value() } }
                                div { label { r#for: "task-sig-prefix", "Prefix" } input { id: "task-sig-prefix", "aria-label": "Prefix", placeholder: "None", value: "{current.signature_prefix}", oninput: move |event| draft.write().signature_prefix = event.value() } }
                                div { label { r#for: "task-sig-encoding", "Encoding" }
                                    select { id: "task-sig-encoding", "aria-label": "Encoding", value: "{current.signature_encoding}", onchange: move |event| draft.write().signature_encoding = if event.value() == "base64" { "base64".into() } else { "hex".into() },
                                        for encoding in ["hex", "base64"] { option { key: "{encoding}", value: "{encoding}", selected: current.signature_encoding == encoding, "{encoding}" } }
                                    } }
                                div { label { r#for: "task-sig-secret", "Secret" }
                                    input { id: "task-sig-secret", r#type: "password", autocomplete: "off", "aria-label": "Secret",
                                        placeholder: if live_task.as_ref().and_then(|task| task.webhook.as_ref()).is_some_and(|webhook| webhook.has_secret) { "Unchanged" } else { "Shared secret" },
                                        value: "{current.signature_secret}", oninput: move |event| draft.write().signature_secret = event.value() } }
                            }
                        }
                    },
                    ScheduleMode::Fixed => rsx! {
                        div { class: "field-row",
                            div { label { r#for: "task-time", "Run at" } input { id: "task-time", r#type: "time", "aria-label": "Run at", value: "{current.time_of_day}", oninput: move |event| draft.write().time_of_day = event.value() } }
                        }
                        div { class: "weekdays", role: "group", "aria-label": "Days to run",
                            for day in logic::WEEKDAY_ORDER {
                                button { key: "{day}", r#type: "button", class: if current.weekdays.contains(&day) { "selected" } else { "" }, "aria-pressed": "{current.weekdays.contains(&day)}", "aria-label": "{logic::weekday_label(day)}", title: "{logic::weekday_label(day)}",
                                    // At least one day stays selected, as in the original.
                                    onclick: move |_| { let mut draft = draft.write(); if draft.weekdays.contains(&day) { if draft.weekdays.len() > 1 { draft.weekdays.remove(&day); } } else { draft.weekdays.insert(day); } },
                                    "{logic::weekday_short(day)}" }
                            }
                        }
                        p { class: "muted", "{logic::TIME_ZONE_NOTE}" }
                    },
                    ScheduleMode::Interval => rsx! {
                        div { class: "field-row",
                            div { label { r#for: "task-interval", "Run every (minutes)" } input { id: "task-interval", r#type: "number", min: "1", step: "any", "aria-label": "Run every", value: "{current.interval_minutes}", oninput: move |event| draft.write().interval_minutes = event.value() } }
                        }
                    },
                }
                label { class: "check", input { r#type: "checkbox", "aria-label": "Enabled", checked: current.enabled, onchange: move |event| draft.write().enabled = event.checked() } "Enabled" }
                p { class: "muted", "Disabled tasks stay saved but do not run." }
            }
            if let Some(message) = problem.read().clone() { p { class: "form-problem", role: "alert", "{message}" } }
            div { class: "dialog-actions",
                button { r#type: "button", "aria-label": "Cancel", disabled: *saving.read(), onclick: move |_| onclose.call(()), "Cancel" }
                button { class: "primary", r#type: "button", "aria-label": if current.editing_id.is_some() { "Save task" } else { "Create task" }, disabled: !can_save, onclick: save, if *saving.read() { "Saving\u{2026}" } else if current.editing_id.is_some() { "Save task" } else { "Create task" } }
            }
        }
    } }
}

/// A webhook task's address, with copy and rotate. The address is the one the original shows.
#[component]
fn WebhookEndpoint(state: Store<UiModel>, transport: TransportHandle, task: Option<ScheduledTask>, can_rotate: bool) -> Element {
    let mut confirming = use_signal(|| false);
    let mut rotating = use_signal(|| false);
    let mut copied = use_signal(|| None::<String>);
    let mut copy_error = use_signal(|| None::<String>);
    let Some((task, endpoint)) = task.as_ref().and_then(|task| task.webhook.as_ref().map(|endpoint| (task, endpoint))) else {
        return rsx! { p { class: "muted", "The URL appears after you save." } };
    };
    let environment = state.destination().peek().as_ref().map(ToString::to_string).unwrap_or_default();
    let base = runtime::http_url(&transport, state, &environment, "/");
    let address = logic::webhook_address(endpoint, base.as_deref());
    let id = task.id.to_string();
    let rotate = {
        let transport = transport.clone();
        move |_| {
            confirming.set(false);
            if *rotating.peek() {
                return;
            }
            let Some(owner) = runtime::connection_owner(&transport, state) else { return };
            rotating.set(true);
            let (transport, id) = (transport.clone(), id.clone());
            spawn(async move {
                let result = runtime::request_value_reported(transport.clone(), state, logic::ROTATE_TOKEN, json!({"id":id})).await;
                rotating.set(false);
                if runtime::connection_owner(&transport, state).as_ref() != Some(&owner) {
                    return;
                }
                if let Err(message) = result {
                    notice::push(state, notice::Level::Error, "Could not rotate webhook URL", message);
                }
            });
        }
    };
    let copy_address = address.address.clone();
    rsx! { div { class: "webhook-endpoint",
        div { class: "webhook-address",
            input { readonly: true, "aria-label": "Webhook URL", value: "{address.address}", onfocus: move |_| { let _ = document::eval("document.activeElement?.select?.();"); } }
            // A bare path is not a URL a sender can call.
            button { r#type: "button", "aria-label": "Copy webhook URL", disabled: !address.copyable, onclick: move |_| {
                    let text = copy_address.clone();
                    spawn(async move { if copy_text(&text).await { copy_error.set(None); copied.set(Some(text)); } else { copy_error.set(Some("Could not copy the webhook URL.".into())); } });
                }, if copied.read().as_deref() == Some(address.address.as_str()) { "Copied" } else { "Copy" } }
            button { r#type: "button", "aria-label": "Rotate webhook URL", disabled: *rotating.read() || !can_rotate, onclick: move |_| confirming.set(true), "Rotate" }
        }
        if let Some(note) = address.note { p { class: "muted", "{note}" } }
        if let Some(message) = copy_error.read().clone() { p { class: "form-problem", role: "alert", "{message}" } }
        if confirming() {
            Confirm { label: "rotate", title: "Rotate this webhook URL?", body: "The current URL stops working.", confirm: "Rotate", busy: !can_rotate, oncancel: move |_| confirming.set(false), onconfirm: rotate }
        }
    } }
}

// ---- the delivery log ----

#[derive(Debug, Clone, PartialEq)]
enum Loaded<T> {
    Loading,
    Ready(T),
    Failed(String),
}

#[component]
fn Deliveries(state: Store<UiModel>, transport: TransportHandle, task: ScheduledTask, onclose: EventHandler<()>) -> Element {
    let mut summaries = use_signal(|| Loaded::<Vec<ScheduledTaskWebhookDeliverySummary>>::Loading);
    let mut selected = use_signal(|| None::<String>);
    let mut detail = use_signal(|| Loaded::<ScheduledTaskWebhookDelivery>::Loading);
    let task_id = task.id.to_string();
    let load = {
        let (transport, task_id) = (transport.clone(), task_id.clone());
        use_callback(move |_: ()| {
            let Some(owner) = runtime::connection_owner(&transport, state) else { return };
            summaries.set(Loaded::Loading);
            let (transport, task_id) = (transport.clone(), task_id.clone());
            spawn(async move {
                let result = runtime::request_value_reported(transport.clone(), state, logic::LIST_DELIVERIES, json!({"id":task_id})).await;
                if runtime::connection_owner(&transport, state).as_ref() != Some(&owner) {
                    return;
                }
                summaries.set(match result.and_then(|value| serde_json::from_value::<t3_contracts::ScheduledTaskListWebhookDeliveriesResult>(value).map_err(|error| format!("Invalid delivery list: {error}"))) {
                    Ok(list) => Loaded::Ready(list.deliveries),
                    Err(message) => Loaded::Failed(message),
                });
            });
        })
    };
    use_hook(|| load.call(()));
    let open = {
        let (transport, task_id) = (transport.clone(), task_id.clone());
        use_callback(move |delivery_id: String| {
            let Some(owner) = runtime::connection_owner(&transport, state) else { return };
            selected.set(Some(delivery_id.clone()));
            detail.set(Loaded::Loading);
            let (transport, task_id) = (transport.clone(), task_id.clone());
            spawn(async move {
                let result = runtime::request_value_reported(transport.clone(), state, logic::GET_DELIVERY, json!({"id":task_id,"deliveryId":delivery_id})).await;
                // An answer for a delivery the user has since left is not wanted.
                if runtime::connection_owner(&transport, state).as_ref() != Some(&owner) || selected.peek().as_deref() != Some(delivery_id.as_str()) {
                    return;
                }
                detail.set(match result.and_then(|value| serde_json::from_value::<t3_contracts::ScheduledTaskGetWebhookDeliveryResult>(value).map_err(|error| format!("Invalid delivery: {error}"))) {
                    Ok(result) => Loaded::Ready(result.delivery),
                    Err(message) => Loaded::Failed(message),
                });
            });
        })
    };
    let title = task.title.to_string();
    rsx! { div { class: "modal-backdrop",
        div { class: "dialog wide-dialog deliveries", role: "dialog", "aria-modal": "true", "aria-label": "Delivery log of {title}", tabindex: "-1",
            onmounted: move |event| { let element = event.data(); spawn(async move { let _ = element.set_focus(true).await; }); },
            onkeydown: move |event| { if event.key() == Key::Escape { event.stop_propagation(); onclose.call(()); } },
            h2 { "Deliveries \u{b7} {title}" }
            p { class: "muted", "Recent requests to this task's webhook URL." }
            match (selected.read().clone(), detail.read().clone(), summaries.read().clone()) {
                (Some(_), Loaded::Loading, _) => rsx! { p { class: "muted", role: "status", "Loading delivery\u{2026}" } },
                (Some(_), Loaded::Failed(message), _) => rsx! { p { class: "form-problem", role: "alert", "{message}" } },
                (Some(_), Loaded::Ready(delivery), _) => rsx! {
                    div { class: "delivery-detail",
                        p { class: "task-status",
                            span { class: logic::delivery_tone(delivery.outcome).class(), "{logic::delivery_outcome_label(delivery.outcome)}" }
                            span { "{delivery.method} \u{b7} {logic::instant_label(&delivery.received_at, &zone)}" }
                            if delivery.signature_verified { span { class: "muted", "Signature verified" } }
                        }
                        if let Some(error) = delivery.error.clone() { p { class: "form-problem", role: "alert", "{error}" } }
                        Block { title: "Prompt sent to the agent", text: delivery.rendered_prompt.clone().unwrap_or_else(|| "No run was started for this request.".into()) }
                        if !delivery.missing_fields.is_empty() { p { class: "muted", "Empty placeholders: {delivery.missing_fields.join(\", \")}" } }
                        Block { title: "Headers", text: delivery.headers.iter().map(|(name, value)| format!("{name}: {value}")).collect::<Vec<_>>().join("\n") }
                        if !delivery.query.is_empty() { Block { title: "Query", text: delivery.query.clone() } }
                        Block { title: if delivery.body_truncated { "Body (truncated)" } else { "Body" }, text: if delivery.body.is_empty() { "(empty)".to_owned() } else { delivery.body.clone() } }
                    }
                },
                (None, _, Loaded::Loading) => rsx! { p { class: "muted", role: "status", "Loading deliveries\u{2026}" } },
                (None, _, Loaded::Failed(message)) => rsx! { p { class: "form-problem", role: "alert", "{message}" } },
                (None, _, Loaded::Ready(deliveries)) if deliveries.is_empty() => rsx! { p { class: "muted", "No requests yet." } },
                (None, _, Loaded::Ready(deliveries)) => rsx! { ul { class: "delivery-list", "aria-label": "Deliveries",
                    for delivery in deliveries {
                        li { key: "{delivery.id}",
                            button { r#type: "button", "aria-label": "Open delivery {delivery.id}", onclick: { let id = delivery.id.to_string(); move |_| open.call(id.clone()) },
                                span { class: logic::delivery_tone(delivery.outcome).class(), "{logic::delivery_outcome_label(delivery.outcome)}" }
                                span { "{delivery.method}" }
                                span { class: "muted", "{logic::instant_label(&delivery.received_at, &zone)}" }
                                if !delivery.missing_fields.is_empty() { span { class: "muted", "{delivery.missing_fields.len()} empty placeholder{plural(delivery.missing_fields.len())}" } }
                            }
                        }
                    }
                } },
            }
            div { class: "dialog-actions",
                if selected.read().is_some() { button { r#type: "button", "aria-label": "Back to deliveries", onclick: move |_| { selected.set(None); }, "Back" } }
                else { button { r#type: "button", "aria-label": "Refresh deliveries", disabled: *summaries.read() == Loaded::Loading, onclick: move |_| load.call(()), "Refresh" } }
                button { class: "primary", r#type: "button", "aria-label": "Done", onclick: move |_| onclose.call(()), "Done" }
            }
        }
    } }
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

#[component]
fn Block(title: String, text: String) -> Element {
    rsx! { div { class: "delivery-block", label { "{title}" } pre { "{text}" } } }
}
