//! Importing projects and their history from Claude Code and Codex: scan the machine for
//! directories an agent has run in, choose some, and bring each in as a project with its past
//! conversations as threads. Port of the import step of `WelcomeWizard.tsx`, as a dialog the
//! palette and the sidebar open at any time.
//!
//! The scan reads the filesystem on the server and says it is looking; it runs when the dialog
//! opens or Retry is pressed, never on a timer. The import runs one project at a time: an
//! existing project (the server's match, else one at the same path) is kept, a missing one is
//! created, then its history is imported into it. Closing the dialog drops whatever the run was
//! still waiting for. Projects that finished stay finished, so a retry skips them.
use crate::{
    provider_manage::{allowed_now, ask},
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
};
use dioxus::prelude::*;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use t3_client::{
    connection::ConnectionStatus,
    project_import::{self as logic, Candidate, Scan},
};
use t3_contracts::AuthEnvironmentScope;

/// Whether the dialog is open.
#[derive(Debug, Clone, Default)]
pub(crate) struct Model {
    pub open: bool,
}

pub(crate) fn open(state: Store<UiModel>) {
    state.import().write().open = true;
}

fn relative(last: &Option<String>) -> String {
    let Some(at) = last.as_deref().and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok()) else { return String::new() };
    let seconds = (chrono::Utc::now() - at.with_timezone(&chrono::Utc)).num_seconds();
    match seconds {
        s if s < 60 => "now".into(),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s if s < 2_592_000 => format!("{}d", s / 86_400),
        s if s < 31_536_000 => format!("{}mo", s / 2_592_000),
        s => format!("{}y", s / 31_536_000),
    }
}
fn source_label(source: &str) -> &'static str {
    if source == "claudeAgent" { "Claude Code" } else { "Codex" }
}

#[derive(Debug, Clone, PartialEq)]
enum Load {
    Looking,
    Failed(String),
    Ready(Scan),
}

/// What a run has settled, kept across retries within the dialog.
#[derive(Debug, Clone, Default)]
struct Run {
    /// Projects whose history imported without anything skipped, by candidate key.
    finished: BTreeMap<String, String>,
    /// Projects with imported threads, by candidate key.
    with_history: BTreeMap<String, String>,
    /// Project creations in flight or failed, by candidate key, so a retry reuses its id.
    attempts: BTreeMap<String, (String, String)>,
}

#[component]
pub(crate) fn Host(state: Store<UiModel>, transport: TransportHandle) -> Element {
    if !state.import().read().open {
        return rsx! {};
    }
    rsx! { Dialog { state, transport } }
}

/// The projects step of the welcome wizard: the dialog's content in the wizard's own frame. `finish` is told where the person
/// lands (the project to start in, if the import made one) or `None` when they chose not to import.
#[component]
pub(crate) fn Step(state: Store<UiModel>, transport: TransportHandle, finish: EventHandler<Option<String>>) -> Element {
    rsx! { Dialog { state, transport, finish } }
}

#[component]
fn Dialog(state: Store<UiModel>, transport: TransportHandle, #[props(default)] finish: Option<EventHandler<Option<String>>>) -> Element {
    let environment = state.destination().read().as_ref().map(ToString::to_string).unwrap_or_default();
    let mut load = use_signal(|| Load::Looking);
    let mut selection = use_signal(|| None::<BTreeSet<String>>);
    let mut importing = use_signal(|| false);
    let mut message = use_signal(String::new);
    let mut run = use_signal(Run::default);
    let mut other_open = use_signal(|| false);
    let mut collapsed = use_signal(BTreeSet::<String>::new);
    // Bumped when the dialog goes, so a run still waiting on the server stops where it is.
    let mut generation = use_signal(|| 0u64);
    use_drop(move || { let next = generation.peek().wrapping_add(1); generation.set(next); });

    let can_read = use_memo(move || allowed_now_read(state));
    let scan = {
        let transport = transport.clone();
        use_callback(move |_: ()| {
            load.set(Load::Looking);
            let transport = transport.clone();
            spawn(async move {
                let answer = ask(&transport, state, logic::SCAN, json!({})).await;
                let Some(answer) = answer else { return };
                load.set(match answer.and_then(|value| Scan::decode(&value)) {
                    Ok(scan) => Load::Ready(scan),
                    Err(message) => Load::Failed(message),
                });
            });
        })
    };
    // The scan starts when the dialog opens with a connection able to read it.
    use_hook({
        let scan = scan.clone();
        move || {
            if allowed_now_read(state) {
                scan.call(());
            }
        }
    });

    let close = move || {
        if !*importing.peek() {
            match finish {
                Some(finish) => finish.call(None),
                None => state.import().write().open = false,
            }
        }
    };
    let candidates: Vec<Candidate> = match &*load.read() {
        Load::Ready(scan) => scan.candidates.clone(),
        _ => Vec::new(),
    };
    let truncated = matches!(&*load.read(), Load::Ready(scan) if scan.truncated);
    let key_of = |candidate: &Candidate| candidate.key(&environment);
    let selected_keys: BTreeSet<String> = selection.read().clone().unwrap_or_else(|| logic::recent(&candidates, chrono::Utc::now().timestamp_millis() as f64).into_iter().map(|candidate| key_of(candidate)).collect());
    let selected: Vec<Candidate> = candidates.iter().filter(|candidate| selected_keys.contains(&key_of(candidate))).cloned().collect();
    let operate = allowed_now(state, AuthEnvironmentScope::OrchestrationOperate);
    let connected = *state.status().read() == ConnectionStatus::Connected;
    let visible_error = if !operate && !selected.is_empty() && connected { logic::PERMISSION_MESSAGE.to_owned() } else { message.read().clone() };

    let set_keys = move |keys: Vec<String>, checked: bool| {
        let mut next = selected_keys.clone();
        for key in keys {
            if checked { next.insert(key); } else { next.remove(&key); }
        }
        selection.set(Some(next));
    };
    let run_import = {
        let (transport, environment, selected) = (transport.clone(), environment.clone(), selected.clone());
        move |_| {
            if *importing.peek() {
                return;
            }
            if !allowed_now(state, AuthEnvironmentScope::OrchestrationOperate) {
                message.set(logic::PERMISSION_MESSAGE.into());
                return;
            }
            if selected.is_empty() {
                match finish {
                    Some(finish) => finish.call(None),
                    None => state.import().write().open = false,
                }
                return;
            }
            importing.set(true);
            message.set(String::new());
            let (transport, environment, selected) = (transport.clone(), environment.clone(), selected.clone());
            let started = *generation.peek();
            spawn(async move {
                let mut imported_threads = 0.0;
                let mut skipped_threads = 0.0;
                let mut complete = 0usize;
                for candidate in &selected {
                    if *generation.peek() != started {
                        return;
                    }
                    let key = candidate.key(&environment);
                    if run.peek().finished.contains_key(&key) {
                        complete += 1;
                        continue;
                    }
                    if !allowed_now(state, AuthEnvironmentScope::OrchestrationOperate) {
                        importing.set(false);
                        message.set(logic::PERMISSION_MESSAGE.into());
                        return;
                    }
                    let projects: Vec<(String, String)> = state.shell().peek().snapshot.as_ref().map(|snapshot| snapshot.projects.iter().map(|project| (project.id.to_string(), project.workspace_root.to_string())).collect()).unwrap_or_default();
                    let project_id = match logic::resolve_project_id(&projects, candidate) {
                        Some(id) => id,
                        None => {
                            let attempt = run.peek().attempts.get(&key).cloned().unwrap_or_else(|| {
                                let id = uuid::Uuid::new_v4().to_string();
                                (id.clone(), format!("onboarding:project:create:{id}"))
                            });
                            run.write().attempts.insert(key.clone(), attempt.clone());
                            let payload = json!({"type":"project.create","commandId":attempt.1,"projectId":attempt.0,"title":candidate.title,"workspaceRoot":candidate.path,"createWorkspaceRootIfMissing":false,"defaultModelSelection":null});
                            let answer = ask(&transport, state, "projects.mutate", payload).await;
                            if *generation.peek() != started {
                                return;
                            }
                            match answer {
                                Some(Ok(_)) => attempt.0,
                                Some(Err(_)) => {
                                    run.write().attempts.remove(&key);
                                    continue;
                                }
                                None => continue,
                            }
                        }
                    };
                    let answer = ask(&transport, state, logic::IMPORT, json!({"projectId":project_id,"expectedWorkspaceRoot":candidate.path})).await;
                    if *generation.peek() != started {
                        return;
                    }
                    match answer {
                        Some(Ok(result)) => {
                            let (done, skipped) = (result["importedCount"].as_f64().unwrap_or(0.0), result["skippedCount"].as_f64().unwrap_or(0.0));
                            imported_threads += done;
                            skipped_threads += skipped;
                            if done > 0.0 {
                                run.write().with_history.insert(key.clone(), project_id.clone());
                            }
                            if skipped == 0.0 {
                                complete += 1;
                                run.write().finished.insert(key.clone(), project_id.clone());
                            }
                        }
                        Some(Err(_)) => {
                            run.write().attempts.remove(&key);
                        }
                        None => {}
                    }
                }
                importing.set(false);
                if complete < selected.len() {
                    message.set(logic::warning(imported_threads, skipped_threads));
                    return;
                }
                // Everything landed: a project with imported history, else any that finished, is where the person starts.
                let landing = selected.iter().find_map(|candidate| run.peek().with_history.get(&candidate.key(&environment)).cloned()).or_else(|| selected.iter().find_map(|candidate| run.peek().finished.get(&candidate.key(&environment)).cloned()));
                if imported_threads > 0.0 {
                    crate::notice::push(state, crate::notice::Level::Success, logic::imported_title(imported_threads), String::new());
                }
                match finish {
                    Some(finish) => finish.call(landing),
                    None => {
                        state.import().write().open = false;
                        if let Some(project) = landing {
                            runtime::new_thread(&transport, state, Some(project));
                        }
                    }
                }
            });
        }
    };

    let (repositories, other) = logic::group(&candidates);
    let selected_count = selected.len();
    let total = candidates.len();
    let other_keys: Vec<String> = other.iter().map(|candidate| key_of(candidate)).collect();
    let other_selected = other_keys.iter().filter(|key| selected_keys_contains(&selection, &candidates, key, &environment)).count();
    let label = logic::import_label(selected_count, *importing.read());
    let other_open_now = *other_open.read();
    let other_fold_label = format!("{} other folders", if other_open_now { "Collapse" } else { "Expand" });
    let other_fold_text = format!("{} Other folders", if other_open_now { "\u{25be}" } else { "\u{25b8}" });
    let other_summary = format!("{} {}", other.len(), if other.len() == 1 { "folder" } else { "folders" });
    let all_keys: Vec<String> = candidates.iter().map(|candidate| key_of(candidate)).collect();
    let embedded = finish.is_some();
    rsx! { div { class: if embedded { "modal-backdrop embedded" } else { "modal-backdrop" }, onclick: move |_| if !embedded { close() },
        div { class: if embedded { "dialog import-dialog embedded" } else { "dialog import-dialog" }, role: if embedded { "group" } else { "dialog" }, "aria-modal": if embedded { "false" } else { "true" }, "aria-label": "Choose projects to import", tabindex: "-1", onclick: move |event| event.stop_propagation(),
            onmounted: move |event| { let element = event.data(); spawn(async move { let _ = element.set_focus(true).await; }); },
            onkeydown: move |event| { if event.key() == Key::Escape { event.stop_propagation(); close(); } },
            h2 { "Choose your projects" }
            p { class: "muted", "Import projects and conversations from Claude Code and Codex on this computer." }
            match load.read().clone() {
                _ if !can_read() => rsx! { p { class: "muted", role: "status", "Connect to an environment to look for projects." } },
                Load::Looking => rsx! { p { class: "muted", role: "status", "Looking for projects from Claude Code and Codex\u{2026}" } },
                Load::Failed(error) => rsx! { div { class: "error-banner", role: "alert", span { "Could not check projects. {error}" } button { r#type: "button", "aria-label": "Retry scan", onclick: move |_| scan.call(()), "Retry" } } },
                Load::Ready(_) => rsx! {
                    if candidates.is_empty() { p { class: "muted", role: "status", "No existing Claude Code or Codex projects found." } }
                    if truncated { p { class: "muted", role: "status", "{logic::SCAN_LIMIT_MESSAGE}" } }
                    if total > 0 {
                        div { class: "import-toolbar",
                            span { role: "status", "{selected_count} of {total} selected" }
                            button { r#type: "button", "aria-label": "Select all", disabled: *importing.read() || selected_count == total, onclick: { let mut set_keys = set_keys.clone(); let keys = all_keys.clone(); move |_| set_keys(keys.clone(), true) }, "Select all" }
                            button { r#type: "button", "aria-label": "Select none", disabled: *importing.read() || selected_count == 0, onclick: { let mut set_keys = set_keys.clone(); let keys = all_keys.clone(); move |_| set_keys(keys.clone(), false) }, "Select none" }
                        }
                    }
                    div { class: "import-list",
                        for group in repositories.iter() {
                            {
                                let keys: Vec<String> = group.candidates.iter().map(|candidate| key_of(candidate)).collect();
                                let chosen = keys.iter().filter(|key| selected_keys_contains(&selection, &candidates, key, &environment)).count();
                                let single = group.candidates.len() == 1;
                                let is_collapsed = collapsed.read().contains(&group.key);
                                let sources: Vec<&str> = { let mut all: Vec<&str> = group.candidates.iter().flat_map(|candidate| candidate.sources.iter().map(String::as_str)).collect(); all.dedup(); all };
                                if single {
                                    let candidate = group.candidates[0];
                                    rsx! { Row { key: "{group.key}", candidate: candidate.clone(), label: group.label.clone(), secondary: if group.repository.is_some() { None } else { Some(candidate.path.clone()) }, nested: false, checked: chosen == 1, disabled: *importing.read(),
                                        onchange: { let mut set_keys = set_keys.clone(); let keys = keys.clone(); move |checked| set_keys(keys.clone(), checked) } } }
                                } else {
                                    let fold_label = format!("{} {}", if is_collapsed { "Expand" } else { "Collapse" }, group.label);
                                let fold_text = format!("{} {}", if is_collapsed { "\u{25b8}" } else { "\u{25be}" }, group.label);
                                let summary = format!("{} \u{b7} {} threads \u{b7} {}", sources.iter().map(|source| source_label(source)).collect::<Vec<_>>().join(", "), group.thread_count, relative(&group.last_active_at));
                                    let group_key = group.key.clone();
                                    rsx! { div { key: "{group.key}", class: "import-group",
                                        label { class: "import-row",
                                            input { r#type: "checkbox", "aria-label": "Select {group.label}", checked: chosen == keys.len(), disabled: *importing.read(), onchange: { let mut set_keys = set_keys.clone(); let keys = keys.clone(); move |event: FormEvent| set_keys(keys.clone(), event.checked()) } }
                                            button { r#type: "button", class: "import-fold", "aria-label": "{fold_label}", "aria-expanded": "{!is_collapsed}", onclick: move |_| { let mut set = collapsed.write(); if !set.remove(&group_key) { set.insert(group_key.clone()); } }, "{fold_text}" }
                                            span { class: "muted import-meta", "{summary}" }
                                        }
                                        if !is_collapsed {
                                            for candidate in group.candidates.iter().copied() {
                                                Row { key: "{candidate.path}", candidate: candidate.clone(), label: candidate.path.clone(), secondary: None, nested: true, checked: selected_keys_contains(&selection, &candidates, &key_of(candidate), &environment), disabled: *importing.read(),
                                                    onchange: { let mut set_keys = set_keys.clone(); let key = key_of(candidate); move |checked| set_keys(vec![key.clone()], checked) } }
                                            }
                                        }
                                    } }
                                }
                            }
                        }
                        if !other.is_empty() {
                            div { class: "import-group",
                                label { class: "import-row",
                                    input { r#type: "checkbox", "aria-label": "Select other folders", checked: other_selected == other.len(), disabled: *importing.read(), onchange: { let mut set_keys = set_keys.clone(); let keys = other_keys.clone(); move |event: FormEvent| set_keys(keys.clone(), event.checked()) } }
                                    button { r#type: "button", class: "import-fold", "aria-label": "{other_fold_label}", "aria-expanded": "{*other_open.read()}", onclick: move |_| other_open.toggle(), "{other_fold_text}" }
                                    span { class: "muted import-meta", "{other_summary}" }
                                }
                                if *other_open.read() {
                                    for candidate in other.iter().copied() {
                                        Row { key: "{candidate.path}", candidate: candidate.clone(), label: candidate.path.clone(), secondary: None, nested: true, checked: selected_keys_contains(&selection, &candidates, &key_of(candidate), &environment), disabled: *importing.read(),
                                            onchange: { let mut set_keys = set_keys.clone(); let key = key_of(candidate); move |checked| set_keys(vec![key.clone()], checked) } }
                                    }
                                }
                            }
                        }
                    }
                },
            }
            if !visible_error.is_empty() { p { class: "error-banner", role: "alert", "{visible_error}" } }
            div { class: "dialog-actions",
                button { r#type: "button", "aria-label": "Do not import projects", disabled: *importing.read(), onclick: move |_| close(), "Do not import projects" }
                button { class: "primary", r#type: "button", "aria-label": "{label}", "aria-busy": "{*importing.read()}", disabled: !operate || *importing.read() || selected_count == 0 || !matches!(&*load.read(), Load::Ready(_)), onclick: run_import, "{label}" }
            }
        }
    } }
}

fn allowed_now_read(state: Store<UiModel>) -> bool {
    *state.status().read() == ConnectionStatus::Connected && state.destination().read().as_ref().is_some_and(|id| state.environments().read().allows(id, AuthEnvironmentScope::OrchestrationRead))
}
/// Whether a key is selected: the person's choice, or the default selection before there is one.
fn selected_keys_contains(selection: &Signal<Option<BTreeSet<String>>>, candidates: &[Candidate], key: &str, environment: &str) -> bool {
    match &*selection.read() {
        Some(keys) => keys.contains(key),
        None => logic::recent(candidates, chrono::Utc::now().timestamp_millis() as f64).into_iter().any(|candidate| candidate.key(environment) == key),
    }
}

#[component]
fn Row(candidate: Candidate, label: String, secondary: Option<String>, nested: bool, checked: bool, disabled: bool, onchange: EventHandler<bool>) -> Element {
    let sources = candidate.sources.iter().map(|source| source_label(source)).collect::<Vec<_>>().join(", ");
    let age = relative(&candidate.last_active_at);
    let imported = candidate.already_imported;
    rsx! { label { class: if nested { "import-row nested" } else { "import-row" }, title: "{candidate.path}",
        input { r#type: "checkbox", "aria-label": "Select {label}", checked, disabled, onchange: move |event| onchange.call(event.checked()) }
        span { class: if nested { "import-label mono" } else { "import-label" }, "{label}" }
        if let Some(path) = secondary { span { class: "muted mono import-secondary", "{path}" } }
        if imported { span { class: "muted import-tag", "Already a project" } }
        span { class: "muted import-meta", if !nested { "{sources} \u{b7} " } "{candidate.thread_count} \u{b7} {age}" }
    } }
}
