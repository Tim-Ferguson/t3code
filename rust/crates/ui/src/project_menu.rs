//! The menu of a project row's header (the original's project context menu in `LegacySidebar.tsx`):
//! rename, group into…, copy the path, project settings and remove. A row that groups several
//! projects offers the four actions that act on one directory once per member, labelled by where it
//! lives (`formatProjectMemberActionLabel`).
use crate::{
    notice::{self, Level},
    runtime::{self, UiModel},
    sidebar_groups, thread_lifecycle,
};
use dioxus::prelude::*;
use serde_json::json;

/// One project of a row.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Member {
    /// The environment the project lives in.
    pub environment: String,
    pub id: String,
    pub title: String,
    pub workspace_root: String,
}

#[derive(Clone, Copy, PartialEq)]
enum Action {
    Rename,
    Group,
    Copy,
    Remove,
}
impl Action {
    fn heading(self) -> &'static str {
        match self {
            Action::Rename => "Rename",
            Action::Group => "Group into\u{2026}",
            Action::Copy => "Copy path",
            Action::Remove => "Remove project",
        }
    }
    /// What the control is called for a project, which a test and a screen reader find it by.
    fn label(self, member: &Member, several: bool) -> String {
        let name = if several { format!("{} ({})", member.title, member.workspace_root) } else { member.title.clone() };
        match self {
            Action::Rename => format!("Rename project {name}"),
            Action::Group => format!("Group project {name}"),
            Action::Copy if several => format!("Copy project path {}", member.workspace_root),
            Action::Copy => "Copy project path".to_owned(),
            Action::Remove => format!("Remove project {name}"),
        }
    }
    /// The text of the item: the action for the only member, the member's place for one of several.
    fn text(self, member: &Member, several: bool) -> String {
        if several { member.workspace_root.clone() } else { self.heading().to_owned() }
    }
}

/// Moves the window to the project's environment, for what is done in the window's own state.
fn open_environment(transport: &runtime::TransportHandle, state: Store<UiModel>, member: &Member) -> bool {
    t3_contracts::EnvironmentId::new(&member.environment).is_ok_and(|environment| crate::fleet::open(transport, state, &environment))
}

#[component]
pub(crate) fn ProjectMenu(state: Store<UiModel>, transport: runtime::TransportHandle, label: String, members: Vec<Member>, can_operate: bool) -> Element {
    let mut open = use_signal(|| false);
    // The member being renamed, and the name typed so far.
    let mut renaming = use_signal(|| None::<(Member, String)>);
    let mut grouping = use_signal(|| None::<Member>);
    let Some(first) = members.first().cloned() else { return rsx! {} };
    let several = members.len() > 1;
    let row_title = label;
    // The name is saved once, by Enter or the button; an empty name is refused and an unchanged one is not sent.
    let save = {
        let transport = transport.clone();
        use_callback(move |typed: String| {
            let Some((member, _)) = renaming.peek().clone() else { return };
            let typed = typed.trim().to_owned();
            if typed.is_empty() {
                notice::push(state, Level::Warning, "Project name cannot be empty", "");
                return;
            }
            renaming.set(None);
            if typed == member.title {
                return;
            }
            // On the connection of the project's own environment, whichever one the window is on.
            let transport = t3_contracts::EnvironmentId::new(&member.environment).map_or_else(|_| transport.clone(), |environment| crate::fleet::link(&transport, &environment));
            spawn(async move {
                let payload = json!({"type":"project.update","commandId":uuid::Uuid::new_v4().to_string(),"projectId":member.id,"title":typed});
                if let Err(reason) = runtime::request_value_reported(transport, state, "projects.mutate", payload).await {
                    notice::push(state, Level::Error, "Could not rename the project", reason);
                }
            });
        })
    };
    // A rename in progress when the connection stops allowing it is dropped.
    if renaming.peek().is_some() && !can_operate {
        renaming.set(None);
    }
    let draft = renaming.read().clone();
    let group_target = grouping.read().clone();
    let item = |action: Action, member: &Member| {
        let (label, text) = (action.label(member, several), action.text(member, several));
        let (member, transport) = (member.clone(), transport.clone());
        let disabled = matches!(action, Action::Rename | Action::Remove) && !can_operate;
        rsx! {
            button { key: "{label}", class: "menu-item", role: "menuitem", r#type: "button", "aria-label": "{label}", disabled,
                onclick: move |_| {
                    open.set(false);
                    match action {
                        Action::Rename => renaming.set(Some((member.clone(), member.title.clone()))),
                        Action::Group => grouping.set(Some(member.clone())),
                        Action::Copy => crate::sidebar_rows::copy_text(&member.workspace_root),
                        Action::Remove => {
                            // The confirmation is the window's: it is asked about on the environment the project lives in.
                            if open_environment(&transport, state, &member) {
                                thread_lifecycle::request_remove_project(state, &member.id);
                            }
                        }
                    }
                },
                "{text}" }
        }
    };
    let settings_member = first.clone();
    let settings_label = format!("Project settings for {row_title}");
    rsx! {
        div { class: "project-menu",
            button { class: "icon-button project-menu-button", r#type: "button", "aria-label": "Project actions for {row_title}", "aria-haspopup": "menu", "aria-expanded": "{open()}", title: "Project actions",
                onclick: move |_| open.set(!open()), "\u{22ef}" }
            if open() {
                div { class: "menu-backdrop", onclick: move |_| open.set(false), oncontextmenu: move |event| { event.prevent_default(); open.set(false); } }
                div { class: "context-menu project-menu-popup", role: "menu", "aria-label": "Project actions", tabindex: "0",
                    onmounted: move |event| { drop(event.set_focus(true)); },
                    onkeydown: move |event| { if event.key() == Key::Escape { event.stop_propagation(); open.set(false); } },
                    for action in [Action::Rename, Action::Group, Action::Copy] {
                        if several { div { key: "{action.heading()}", class: "menu-heading", role: "presentation", "{action.heading()}" } }
                        for member in members.iter() { { item(action, member) } }
                    }
                    button { class: "menu-item", role: "menuitem", r#type: "button", "aria-label": "{settings_label}",
                        onclick: { let transport = transport.clone(); move |_| { open.set(false); if open_environment(&transport, state, &settings_member) { crate::projects_page::open(state, &settings_member.id); } } }, "Project settings" }
                    if several { div { class: "menu-heading", role: "presentation", "Remove project" } }
                    for member in members.iter() { { item(Action::Remove, member) } }
                }
            }
            if let Some((member, typed)) = draft {
                div { class: "modal-backdrop", onclick: move |_| renaming.set(None),
                    div { class: "dialog", role: "dialog", "aria-modal": "true", "aria-label": "Rename project", onclick: move |event| event.stop_propagation(),
                        onkeydown: move |event| { if event.key() == Key::Escape { event.stop_propagation(); renaming.set(None); } },
                        h2 { "Rename project" }
                        if several { p { class: "muted", "{member.workspace_root}" } }
                        input { "aria-label": "Project name", value: "{typed}", onmounted: move |event| { drop(event.set_focus(true)); },
                            oninput: move |event| { let current = renaming.peek().clone(); if let Some((member, _)) = current { renaming.set(Some((member, event.value()))); } },
                            onkeydown: move |event| { if event.key() == Key::Enter { event.prevent_default(); let typed = renaming.peek().clone().map(|(_, typed)| typed).unwrap_or_default(); save.call(typed); } } }
                        div { class: "dialog-actions",
                            button { r#type: "button", "aria-label": "Cancel rename", onclick: move |_| renaming.set(None), "Cancel" }
                            button { class: "primary", r#type: "button", "aria-label": "Save project name", onclick: move |_| { let typed = renaming.peek().clone().map(|(_, typed)| typed).unwrap_or_default(); save.call(typed); }, "Save" }
                        }
                    }
                }
            }
            if let Some(member) = group_target {
                sidebar_groups::GroupingDialog { state, override_key: sidebar_groups::override_key(&member.environment, &member.workspace_root), workspace_root: member.workspace_root.clone(), close: move |()| grouping.set(None) }
            }
        }
    }
}
