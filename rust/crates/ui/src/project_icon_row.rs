//! The "Project icon" row of the Projects settings page: what the project's icon is, a button that chooses an image of the project (or,
//! in the desktop app, of this computer) for it, and a reset. Port of the row in `ProjectSettingsPanel.tsx` and of
//! `ProjectFaviconPickerDialog.tsx`. The desktop app adds the footer button that opens the system's file dialog (`pickProjectFavicon`);
//! a browser has no such button. The picker of icons, emoji and monograms (`ProjectIconPickerDialog.tsx`) is not built.
use crate::{
    notice::{self, Level},
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
    settings_common::connected_allows,
};
use dioxus::prelude::*;
use futures_util::future::LocalBoxFuture;
use serde_json::{Value, json};
use std::rc::Rc;
use t3_client::{
    connection::ConnectionStatus,
    project_files::{self as files, FileMatch},
};
use t3_contracts::{AuthEnvironmentScope, ProjectIcon};

/// What the desktop app adds: the system's file dialog, for an image of this computer.
pub(crate) trait Picker {
    /// Whether the button is offered for the project at `workspace_root`: its folder is on this computer, and the window is on the server
    /// that runs here (`pickProjectFavicon` is only given to projects of the primary environment).
    fn offered(&self, transport: &TransportHandle, workspace_root: &str) -> bool;
    /// What the button calls the program that shows files ("Finder").
    fn manager_name(&self) -> &'static str;
    /// The file the person chose, none when they cancelled. Err is a message to show.
    fn pick(&self, workspace_root: &str) -> LocalBoxFuture<'static, Result<Option<String>, String>>;
}

#[derive(Clone)]
pub(crate) struct PickerHandle(pub Rc<dyn Picker>);

/// `projectIcon?.kind === ... ? ... : faviconPath ?? "Automatic"`: what the row says under its title.
pub(crate) fn describe(icon: Option<&ProjectIcon>, favicon_path: Option<&str>) -> String {
    match icon {
        Some(ProjectIcon::Lucide { name, color }) => format!("{name} \u{b7} {}", color_name(*color)),
        Some(ProjectIcon::Monogram { text, color }) => format!("{text} \u{b7} {}", color_name(*color)),
        Some(ProjectIcon::Emoji { emoji }) => emoji.clone(),
        None => favicon_path.unwrap_or("Automatic").to_owned(),
    }
}

fn color_name(color: t3_contracts::ProjectIconColor) -> String {
    serde_json::to_value(color).ok().and_then(|value| value.as_str().map(str::to_owned)).unwrap_or_default()
}

/// What the row reads of the project.
struct Shown {
    title: String,
    workspace_root: String,
    favicon_path: Option<String>,
    icon: Option<ProjectIcon>,
}

fn shown(state: Store<UiModel>, id: &str) -> Option<Shown> {
    let shell = state.shell();
    let shell = shell.read();
    let project = shell.snapshot.as_ref()?.projects.iter().find(|project| project.id.as_str() == id)?;
    Some(Shown { title: project.title.to_string(), workspace_root: project.workspace_root.to_string(), favicon_path: project.favicon_path().map(str::to_owned), icon: project.project_icon() })
}

/// `project.update` of the icon fields: the file chosen, or nothing for the automatic icon. The icon of the other kinds is cleared too,
/// as a chosen file replaces them.
fn update_input(project_id: &str, favicon_path: Option<&str>) -> Value {
    json!({"type":"project.update","commandId":uuid::Uuid::new_v4().to_string(),"projectId":project_id,"faviconPath":favicon_path,"projectIcon":null})
}

#[component]
pub(crate) fn Row(state: Store<UiModel>, transport: TransportHandle, id: String) -> Element {
    let Some(project) = shown(state, &id) else { return rsx! {} };
    let can = connected_allows(state, AuthEnvironmentScope::OrchestrationOperate);
    let mut saving = use_signal(|| false);
    let mut choosing = use_signal(|| false);
    let environment = state.destination().read().as_ref().map(ToString::to_string).unwrap_or_default();
    let mut set_icon = {
        let (id, transport) = (id.clone(), transport.clone());
        move |favicon_path: Option<String>| {
            if *saving.peek() {
                return;
            }
            saving.set(true);
            let (payload, transport) = (update_input(&id, favicon_path.as_deref()), transport.clone());
            spawn(async move {
                if let Err(reason) = runtime::request_value_reported(transport, state, "projects.mutate", payload).await {
                    notice::push(state, Level::Error, "Failed to update project icon", reason);
                }
                saving.set(false);
            });
        }
    };
    let overridden = project.favicon_path.is_some() || project.icon.is_some();
    let description = describe(project.icon.as_ref(), project.favicon_path.as_deref());
    rsx! {
        div { class: "setting-row", id: "project-icon",
            div { class: "font-description", strong { "Project icon" } p { class: "muted", "{description}" } }
            div { class: "font-controls",
                match &project.icon {
                    Some(ProjectIcon::Emoji { emoji }) => rsx! { span { class: "project-monogram", "aria-hidden": "true", "{emoji}" } },
                    Some(ProjectIcon::Monogram { text, .. }) => rsx! { span { class: "project-monogram", "aria-hidden": "true", "{text}" } },
                    _ => rsx! { crate::file_preview::ProjectFavicon { key: "{project.favicon_path:?}", state, transport: transport.clone(), environment_key: environment.clone(), cwd: project.workspace_root.clone(), title: project.title.clone() } },
                }
                button { r#type: "button", "aria-label": "Choose a project icon file", disabled: *saving.read() || !can, onclick: move |_| choosing.set(true), "Choose file" }
                if overridden {
                    button { r#type: "button", "aria-label": "Reset project icon to default", disabled: *saving.read() || !can, onclick: { let mut set_icon = set_icon.clone(); move |_| set_icon(None) }, "Reset" }
                }
            }
        }
        if *choosing.read() && can {
            FilePicker { key: "{project.workspace_root}", state, transport, cwd: project.workspace_root.clone(), title: project.title.clone(), onclose: move |()| choosing.set(false), onselect: move |path: String| { choosing.set(false); set_icon(Some(path)); } }
        }
    }
}

const NO_ACCESS: &str = "This connection cannot search host files.";

/// What the server last said about one query.
struct Answered {
    query: String,
    rows: Result<Rc<Vec<FileMatch>>, String>,
}

fn search_input(cwd: &str, query: &str) -> Value {
    let mut input = files::entries_input(cwd, query);
    input["imageOnly"] = json!(true);
    input
}

/// Text with the characters the query matched in strong type.
#[component]
fn Marked(text: String, indices: Vec<usize>, active: bool) -> Element {
    if !active || indices.is_empty() {
        return rsx! { "{text}" };
    }
    let runs = files::marked_runs(&text, &indices);
    rsx! { for (position, (run, marked)) in runs.into_iter().enumerate() { if marked { strong { key: "{position}", "{run}" } } else { "{run}" } } }
}

/// `ProjectFaviconPickerDialog`: the image files of the project, searched by name, and in the desktop app the system's file dialog.
#[component]
fn FilePicker(state: Store<UiModel>, transport: TransportHandle, cwd: String, title: String, onclose: EventHandler<()>, onselect: EventHandler<String>) -> Element {
    let picker = try_consume_context::<PickerHandle>().filter(|picker| picker.0.offered(&transport, &cwd));
    let mut query = use_signal(String::new);
    let mut highlighted = use_signal(|| 0usize);
    let mut answer = use_signal(|| None::<Answered>);
    let mut picking = use_signal(|| false);
    let first = use_hook(|| Rc::new(std::cell::Cell::new(true)));
    let allowed = use_memo(move || *state.status().read() == ConnectionStatus::Connected && state.destination().read().as_ref().is_some_and(|destination| state.environments().read().allows(destination, AuthEnvironmentScope::FilesystemRead)));
    use_resource({
        let (transport, cwd) = (transport.clone(), cwd.clone());
        move || {
            let typed = query.read().trim().to_owned();
            let allowed = allowed();
            let (transport, cwd, first) = (transport.clone(), cwd.clone(), first.clone());
            async move {
                if !allowed {
                    return;
                }
                // The first question goes at once; later ones wait for a pause in the typing.
                if !first.replace(false) {
                    runtime::delay(files::PATH_DEBOUNCE_MS).await;
                }
                let owner = runtime::response_owner(&transport, state);
                let result = runtime::request_value_reported(transport.clone(), state, files::SEARCH_ENTRIES, search_input(&cwd, &typed)).await;
                if owner.is_none() || owner != runtime::response_owner(&transport, state) {
                    return;
                }
                let rows = result.and_then(|value| files::decode_entries(&value)).map(|found| Rc::new(files::file_matches(&found.entries, &typed, files::FILE_LIMIT)));
                highlighted.set(0);
                answer.set(Some(Answered { query: typed, rows }));
            }
        }
    });

    let typed = query.read().clone();
    let pending = allowed() && answer.read().as_ref().is_none_or(|answered| answered.query != typed.trim());
    let (rows, error) = match &*answer.read() {
        Some(Answered { rows: Ok(rows), .. }) if !pending => (rows.clone(), None),
        Some(Answered { rows: Err(message), .. }) if !pending => (Rc::new(vec![]), Some(message.clone())),
        _ => (Rc::new(vec![]), None),
    };
    let error = if allowed() { error } else { Some(NO_ACCESS.to_owned()) };
    let message = empty_message(&typed, error.as_deref(), pending && allowed());
    let marked_query = answer.read().as_ref().map(|answered| answered.query.clone()).unwrap_or_default();
    let count = rows.len();
    let shown = (*highlighted.read()).min(count.saturating_sub(1));
    let chosen = {
        let rows = rows.clone();
        move |position: usize| {
            if let Some(row) = rows.get(position) {
                onselect.call(row.path.clone());
            }
        }
    };
    let pick_external = picker.clone().map(|picker| {
        let cwd = cwd.clone();
        move || {
            if *picking.peek() {
                return;
            }
            picking.set(true);
            let chosen = picker.0.pick(&cwd);
            spawn(async move {
                match chosen.await {
                    Ok(Some(path)) => onselect.call(path),
                    Ok(None) => {}
                    Err(reason) => notice::push(state, Level::Error, "Could not open image picker", reason),
                }
                picking.set(false);
            });
        }
    });
    rsx! {
        div { class: "modal-backdrop palette-backdrop", onclick: move |_| onclose.call(()),
            div { class: "palette palette-tall", role: "dialog", "aria-modal": "true", "aria-label": "Choose project icon", onclick: move |event| event.stop_propagation(),
                div { class: "palette-input",
                    input { r#type: "text", role: "combobox", "aria-label": "Search image files\u{2026}", "aria-expanded": "true", "aria-controls": "project-icon-results", "aria-autocomplete": "list", "aria-activedescendant": if count > 0 { Some(format!("project-icon-row-{shown}")) } else { None }, placeholder: "Search image files\u{2026}", autocomplete: "off", spellcheck: "false", value: "{typed}",
                        onmounted: move |event| { spawn(async move { let _ = event.set_focus(true).await; }); },
                        oninput: move |event| { query.set(event.value()); },
                        onkeydown: {
                            let chosen = chosen.clone();
                            move |event| {
                                if event.is_composing() { return; }
                                match event.key() {
                                    Key::Escape => { event.prevent_default(); event.stop_propagation(); onclose.call(()); }
                                    Key::ArrowDown if count > 0 => { event.prevent_default(); highlighted.set((shown + 1) % count); }
                                    Key::ArrowUp if count > 0 => { event.prevent_default(); highlighted.set((shown + count - 1) % count); }
                                    Key::Enter => { event.prevent_default(); chosen(shown); }
                                    _ => {}
                                }
                            }
                        } }
                }
                if rows.is_empty() {
                    p { class: "palette-empty", role: "status", "{message}" }
                } else {
                    ul { id: "project-icon-results", class: "palette-results", role: "listbox", "aria-label": "Image files",
                        li { class: "palette-group", role: "presentation", "{title}" }
                        for (position, row) in rows.iter().enumerate() {
                            li { key: "{row.path}", role: "presentation",
                                button { id: "project-icon-row-{position}", class: if position == shown { "palette-row active" } else { "palette-row" }, r#type: "button", role: "option", tabindex: "-1", "aria-label": "{row.path}", "aria-selected": "{position == shown}",
                                    onmousedown: move |event| event.prevent_default(),
                                    onmousemove: move |_| { if *highlighted.peek() != position { highlighted.set(position); } },
                                    onclick: { let chosen = chosen.clone(); move |_| chosen(position) },
                                    span { class: "palette-text",
                                        span { class: "palette-title", Marked { text: row.name.clone(), indices: row.name_indices.clone(), active: !marked_query.trim().is_empty() } }
                                        span { class: "palette-description", Marked { text: row.path.clone(), indices: row.path_indices.clone(), active: !marked_query.trim().is_empty() } }
                                    }
                                }
                            }
                        }
                    }
                }
                div { class: "palette-footer",
                    span { "Select icon" }
                    if let (Some(picker), Some(mut pick)) = (&picker, pick_external) {
                        button { r#type: "button", "aria-label": "Open in {picker.0.manager_name()}", disabled: *picking.read(), onclick: move |_| pick(), "Open in {picker.0.manager_name()}" }
                    }
                }
            }
        }
    }
}

/// `emptyMessage`: what the picker says in place of rows.
fn empty_message(query: &str, error: Option<&str>, pending: bool) -> String {
    if let Some(error) = error {
        return error.to_owned();
    }
    match (pending, !query.trim().is_empty()) {
        (true, true) => "Searching project files\u{2026}".into(),
        (true, false) => "Indexing project files\u{2026}".into(),
        (false, true) => "No matching image files.".into(),
        (false, false) => "No image files found.".into(),
    }
}

#[cfg(test)]
#[path = "project_icon_row_tests.rs"]
mod tests;
