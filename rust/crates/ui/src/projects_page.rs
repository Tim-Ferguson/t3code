//! The Projects settings page: one project's overview (name, folder, remove) and the defaults it
//! overrides. Port of `ProjectsSettings.tsx` and `ProjectSettingsPanel.tsx`; reached from the
//! sidebar's project menu, a thread's menu and the command palette ("Project settings").
use crate::{
    notice::{self, Level},
    project_defaults::ScopedRows,
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt, View},
    settings_common::{Tabs, connected_allows},
    sidebar_rows::copy_text,
    thread_lifecycle,
};
use dioxus::prelude::*;
use serde_json::json;
use t3_contracts::AuthEnvironmentScope;

/// Opens the page on a project.
pub(crate) fn open(state: Store<UiModel>, project_id: &str) {
    state.settings_project().set(Some(project_id.to_owned()));
    state.view().set(View::Projects);
}

#[component]
pub(crate) fn Page(state: Store<UiModel>, transport: TransportHandle) -> Element {
    let projects: Vec<(String, String, String)> = state.shell().read().snapshot.as_ref().map(|snapshot| snapshot.projects.iter().map(|project| (project.id.to_string(), project.title.to_string(), project.workspace_root.to_string())).collect()).unwrap_or_default();
    let chosen = state.settings_project().read().clone().filter(|id| projects.iter().any(|(project, ..)| project == id)).or_else(|| projects.first().map(|(id, ..)| id.clone()));
    rsx! {
        section { class: "settings-page", Tabs { state, current: View::Projects }
            h1 { "Projects" }
            p { class: "muted", "Rename a project, see where it lives, and choose what new threads in it start with." }
            if projects.is_empty() {
                p { class: "muted", role: "status", "Add a project to change its settings." }
            } else {
                div { class: "field-row",
                    label { r#for: "settings-project", "Project" }
                    select { id: "settings-project", "aria-label": "Project", value: "{chosen.clone().unwrap_or_default()}", onchange: move |event| state.settings_project().set(Some(event.value())),
                        for (id, title, _) in projects.iter() { option { key: "{id}", value: "{id}", selected: chosen.as_deref() == Some(id.as_str()), "{title}" } }
                    }
                }
                if let Some((id, title, path)) = chosen.as_deref().and_then(|chosen| projects.iter().find(|(id, ..)| id == chosen)).cloned() {
                    Overview { key: "overview:{id}", state, transport: transport.clone(), id: id.clone(), title, path }
                    div { id: "project-defaults",
                        ScopedRows { key: "defaults:{id}", state, transport: transport.clone(), project: Some(id.clone()) }
                    }
                    crate::project_actions::Section { key: "actions:{id}", state, transport: transport.clone(), project: Some(id) }
                }
            }
        }
    }
}

#[component]
fn Overview(state: Store<UiModel>, transport: TransportHandle, id: String, title: String, path: String) -> Element {
    let mut name = use_signal(|| title.clone());
    let can = connected_allows(state, AuthEnvironmentScope::OrchestrationOperate);
    let threads = state.shell().read().snapshot.as_ref().map(|snapshot| snapshot.threads.iter().filter(|thread| thread.project_id.as_str() == id && thread.deleted_at.is_none()).count()).unwrap_or(0);
    let save = {
        let (id, original, transport) = (id.clone(), title.clone(), transport.clone());
        move || {
            let typed = name.peek().trim().to_owned();
            if typed.is_empty() {
                notice::push(state, Level::Warning, "Project name cannot be empty", "");
                return;
            }
            if typed == original {
                return;
            }
            let (id, transport) = (id.clone(), transport.clone());
            spawn(async move {
                let payload = json!({"type":"project.update","commandId":uuid::Uuid::new_v4().to_string(),"projectId":id,"title":typed});
                if let Err(reason) = runtime::request_value_reported(transport, state, "projects.mutate", payload).await {
                    notice::push(state, Level::Error, "Could not rename the project", reason);
                }
            });
        }
    };
    rsx! {
        section { class: "sc-section", id: "project-overview", "aria-label": "Project overview",
            h2 { "Project overview" }
            div { class: "setting-row",
                div { class: "font-description", strong { "Name" } p { class: "muted", "Shown in the sidebar and the command palette." } }
                div { class: "font-controls",
                    input { "aria-label": "Project name", value: "{name}", disabled: !can, oninput: move |event| name.set(event.value()),
                        onkeydown: { let save = save.clone(); move |event| { if event.key() == Key::Enter { event.prevent_default(); save(); } } } }
                    button { r#type: "button", "aria-label": "Save project name", disabled: !can || name.read().trim() == title, onclick: { let save = save.clone(); move |_| save() }, "Save" }
                }
            }
            div { class: "setting-row",
                div { class: "font-description", strong { "Folder" } p { class: "muted", "{path}" } }
                div { class: "font-controls", button { r#type: "button", "aria-label": "Copy project path", onclick: { let path = path.clone(); move |_| copy_text(&path) }, "Copy path" } }
            }
            div { class: "setting-row",
                div { class: "font-description", strong { "Remove project" } p { class: "muted", "Takes the project out of T3 Code. {threads} thread(s) in it are deleted; the folder stays on disk." } }
                div { class: "font-controls", button { class: "destructive", r#type: "button", "aria-label": "Remove project {title}", disabled: !can, onclick: { let id = id.clone(); move |_| thread_lifecycle::request_remove_project(state, &id) }, "Remove project" } }
            }
        }
    }
}
