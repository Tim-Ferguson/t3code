//! Grouped project rows in the sidebar: which projects share a row (`t3_client::project_grouping`, the
//! original's `buildProjectGroups`), the "Group into…" dialog of a project's menu, and the sidebar's
//! "Filter threads by project" control. A row groups the checkouts and worktrees of one repository across every
//! connected environment, as the original's does: a repository open on two machines is one row, and the row's
//! threads are both machines'.
use crate::{
    fleet,
    runtime::{UiModel, UiModelStoreExt},
    settings_rows,
};
use dioxus::prelude::*;
use serde_json::json;
use t3_client::project_grouping::{self as grouping, Group, Mode};
use t3_contracts::{EnvironmentId, ProjectShell};

const SCOPE_KEY: &str = "t3code:sidebar:project-scope";
const LAST_MODE_KEY: &str = "t3code:last-enabled-project-grouping-mode";

/// The projects a row lists the threads of: a group's members, or one project, each as `(environment, project id)`. `key` names
/// the row wherever a row is remembered (a held drop): the environment and id of the group's representative project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Scope {
    pub key: String,
    pub refs: Vec<(String, String)>,
}
impl Scope {
    #[cfg(test)]
    pub fn single(environment: &str, id: &str) -> Self {
        Self { key: format!("{environment}:{id}"), refs: vec![(environment.to_owned(), id.to_owned())] }
    }
    fn alone(environment: &EnvironmentId, id: &str) -> Self {
        Self { key: fleet::key(environment, id), refs: vec![(environment.to_string(), id.to_owned())] }
    }
    pub fn contains(&self, environment: &EnvironmentId, project: &str) -> bool {
        self.refs.iter().any(|(env, id)| env == environment.as_str() && id == project)
    }
    /// The environment the row's projects are all in, when they are.
    pub fn environment(&self) -> Option<&str> {
        let first = self.refs.first()?.0.as_str();
        self.refs.iter().all(|(environment, _)| environment == first).then_some(first)
    }
}

pub(crate) fn project_of(environment: &EnvironmentId, project: &ProjectShell) -> grouping::Project {
    grouping::Project {
        environment: environment.to_string(),
        id: project.id.to_string(),
        title: project.title.to_string(),
        workspace_root: project.workspace_root.to_string(),
        created_at: project.created_at.to_string(),
        updated_at: project.updated_at.to_string(),
        identity: project.extra.get("repositoryIdentity").filter(|identity| identity.is_object()).cloned(),
    }
}

/// The sidebar's rows, in the order their first project appears; the open environment's project stands for a row it is in.
pub(crate) fn groups_of(shells: &[fleet::Shell<'_>], settings: &t3_contracts::ClientSettings, open: Option<&EnvironmentId>) -> (Vec<grouping::Project>, Vec<Group>) {
    let inputs: Vec<_> = shells.iter().flat_map(|shell| shell.snapshot.projects.iter().map(|project| project_of(shell.environment, project))).collect();
    let groups = grouping::build_groups(&inputs, &grouping::Settings::of(settings), open.map(EnvironmentId::as_str));
    (inputs, groups)
}

pub(crate) fn scope_of_group(inputs: &[grouping::Project], group: &Group) -> Scope {
    let representative = &inputs[group.representative];
    Scope { key: format!("{}:{}", representative.environment, representative.id), refs: group.project_refs.clone() }
}

/// One row of the sidebar, as it is drawn.
pub(crate) struct Row {
    /// The logical key of the group, which the project filter names.
    pub key: String,
    pub label: String,
    pub scope: Scope,
    pub representative: grouping::Project,
    /// The physical projects of the row, one for each directory of each environment.
    pub members: Vec<grouping::Project>,
}

/// Every row of the sidebar, in order, before the project filter.
pub(crate) fn rows(state: Store<UiModel>) -> Vec<Row> {
    let open = state.destination().read().clone();
    let settings = state.client_settings();
    let settings = settings.read();
    fleet::read(state, |shells| {
        let (inputs, groups) = groups_of(shells, &settings, open.as_ref());
        groups
            .iter()
            .map(|group| Row {
                key: group.key.clone(),
                label: group.label.clone(),
                scope: scope_of_group(&inputs, group),
                representative: inputs[group.representative].clone(),
                members: group.members.iter().map(|member| inputs[member.index].clone()).collect(),
            })
            .collect()
    })
}

/// The row a project is listed in.
pub(crate) fn scope_of(state: Store<UiModel>, environment: &EnvironmentId, project_id: &str) -> Scope {
    let open = state.destination().read().clone();
    let settings = state.client_settings();
    let settings = settings.read();
    fleet::read(state, |shells| {
        let (inputs, groups) = groups_of(shells, &settings, open.as_ref());
        match grouping::group_of(&groups, environment.as_str(), project_id) {
            Some(group) => scope_of_group(&inputs, group),
            None => Scope::alone(environment, project_id),
        }
    })
}

/// The rows the sidebar shows, in order: all of them, or the one the project filter is on.
pub(crate) fn visible_scopes(state: Store<UiModel>) -> Vec<Scope> {
    let open = state.destination().read().clone();
    let settings = state.client_settings();
    let settings = settings.read();
    fleet::read(state, |shells| {
        let (inputs, groups) = groups_of(shells, &settings, open.as_ref());
        let scoped = scoped_key(state, groups.iter().map(|group| group.key.as_str()));
        groups.iter().filter(|group| scoped.as_ref().is_none_or(|key| *key == group.key)).map(|group| scope_of_group(&inputs, group)).collect()
    })
}

/// The key of the project group chosen in the filter, if it still exists.
pub(crate) fn scoped_key<'a>(state: Store<UiModel>, mut keys: impl Iterator<Item = &'a str>) -> Option<String> {
    state.project_scope().read().clone().filter(|scoped| keys.any(|key| key == scoped))
}

async fn read_text(key: &'static str) -> Option<String> {
    let key = serde_json::to_string(key).expect("key");
    document::eval(&format!("try{{return window.localStorage.getItem({key});}}catch{{return null;}}")).join::<Option<String>>().await.ok().flatten()
}
fn write_text(key: &'static str, value: Option<&str>) {
    let key = serde_json::to_string(key).expect("key");
    let script = match value {
        Some(value) => format!("try{{window.localStorage.setItem({key},{});}}catch{{}}", serde_json::to_string(value).expect("value")),
        None => format!("try{{window.localStorage.removeItem({key});}}catch{{}}"),
    };
    document::eval(&script);
}

/// Reads the project the filter was left on, once. The browser remembers it as the original's UI store does.
pub(crate) fn use_scope(state: Store<UiModel>) {
    use_hook(move || {
        spawn(async move {
            if let Some(key) = read_text(SCOPE_KEY).await.filter(|key| !key.is_empty()) {
                if state.project_scope().peek().is_none() {
                    state.project_scope().set(Some(key));
                }
            }
        })
    });
}
pub(crate) fn set_scope(state: Store<UiModel>, key: Option<String>) {
    write_text(SCOPE_KEY, key.as_deref());
    state.project_scope().set(key);
    state.selection().write().clear();
}

/// The mode the grouping switch returns to when it is turned back on (`readLastEnabledProjectGroupingMode`).
pub(crate) async fn last_enabled_mode() -> Mode {
    if read_text(LAST_MODE_KEY).await.as_deref() == Some("repository_path") { Mode::RepositoryPath } else { Mode::Repository }
}
pub(crate) fn remember_enabled_mode(mode: Mode) {
    if mode != Mode::Separate {
        write_text(LAST_MODE_KEY, Some(grouping::mode_wire(mode)));
    }
}

/// The Project grouping switch (`SettingsPanels.tsx`).
#[component]
pub(crate) fn GroupingSwitch(state: Store<UiModel>) -> Element {
    let mode = state.client_settings().read().sidebar_project_grouping_mode;
    let default = t3_contracts::ClientSettings::default().sidebar_project_grouping_mode;
    let mut last = use_signal(|| Mode::Repository);
    use_hook(move || {
        spawn(async move {
            let remembered = last_enabled_mode().await;
            last.set(remembered);
        })
    });
    rsx! {
        div { class: "setting-row", "data-setting": "project-grouping",
            label { class: "switch-row",
                input { r#type: "checkbox", role: "switch", "aria-label": "Project grouping", checked: grouping::enabled(mode),
                    onchange: move |event| {
                        let current = state.client_settings().peek().sidebar_project_grouping_mode;
                        if !event.checked() && current != Mode::Separate {
                            last.set(current);
                            remember_enabled_mode(current);
                        }
                        let next = grouping::mode_from_toggle(event.checked(), *last.peek());
                        settings_rows::patch(state, json!({"sidebarProjectGroupingMode": grouping::mode_wire(next)}));
                    } }
                " Project grouping" }
            p { class: "muted", "Combine matching repositories across environments." }
            if mode != default {
                button { class: "subtle", r#type: "button", "aria-label": "Reset project grouping",
                    onclick: move |_| settings_rows::patch(state, json!({"sidebarProjectGroupingMode": grouping::mode_wire(default)})), "Reset" }
            }
        }
    }
}

/// "Group into…": how one directory is grouped, over the global default.
#[component]
pub(crate) fn GroupingDialog(state: Store<UiModel>, workspace_root: String, override_key: String, close: EventHandler<()>) -> Element {
    let settings = state.client_settings().read().clone();
    let global = settings.sidebar_project_grouping_mode;
    let initial = settings.sidebar_project_grouping_overrides.iter().find(|(existing, _)| existing.as_str() == override_key).map(|(_, mode)| *mode);
    let mut choice = use_signal(move || initial);
    let described = choice().unwrap_or(global);
    let save_key = override_key.clone();
    rsx! {
        div { class: "modal-backdrop", onclick: move |_| close.call(()),
            div { class: "dialog", role: "dialog", "aria-modal": "true", "aria-label": "Group project", onclick: move |event| event.stop_propagation(),
                onkeydown: move |event| { if event.key() == Key::Escape { event.stop_propagation(); close.call(()); } },
                h2 { "Project grouping" }
                p { class: "muted", "Choose how {workspace_root} should be grouped in the sidebar." }
                label { r#for: "grouping-rule", "Grouping rule" }
                select { id: "grouping-rule", "aria-label": "Project grouping rule",
                    onchange: move |event| choice.set(match event.value().as_str() { "repository" => Some(Mode::Repository), "repository_path" => Some(Mode::RepositoryPath), "separate" => Some(Mode::Separate), _ => None }),
                    option { value: "inherit", selected: choice().is_none(), "Use global default ({grouping::mode_label(global)})" }
                    for mode in [Mode::Repository, Mode::RepositoryPath, Mode::Separate] {
                        option { key: "{grouping::mode_wire(mode)}", value: "{grouping::mode_wire(mode)}", selected: choice() == Some(mode), "{grouping::mode_label(mode)}" }
                    }
                }
                p { class: "muted", "{grouping::mode_description(described)}" }
                div { class: "dialog-actions",
                    button { r#type: "button", "aria-label": "Cancel grouping", onclick: move |_| close.call(()), "Cancel" }
                    button { class: "primary", r#type: "button", "aria-label": "Save grouping",
                        onclick: move |_| {
                            let current = state.client_settings().peek().sidebar_project_grouping_overrides.iter().map(|(key, mode)| (key.to_string(), *mode)).collect();
                            let next = grouping::overrides_with(&current, &save_key, choice());
                            let wire: serde_json::Map<String, serde_json::Value> = next.iter().map(|(key, mode)| (key.clone(), json!(grouping::mode_wire(*mode)))).collect();
                            settings_rows::patch(state, json!({"sidebarProjectGroupingOverrides": wire}));
                            close.call(());
                        }, "Save" }
                }
            }
        }
    }
}

/// The sidebar's "Filter threads by project" control: a button that opens a searchable list of the rows.
#[component]
pub(crate) fn ScopeFilter(state: Store<UiModel>, items: Vec<(String, String)>) -> Element {
    let mut open = use_signal(|| false);
    let mut query = use_signal(String::new);
    let scoped = scoped_key(state, items.iter().map(|(key, _)| key.as_str()));
    let scoped_label = scoped.as_ref().and_then(|key| items.iter().find(|(item, _)| item == key)).map(|(_, label)| label.clone());
    let label = scoped_label.as_ref().map_or_else(|| "Filter threads by project".to_owned(), |name| format!("Filter threads by project: {name}"));
    let needle = query.read().trim().to_lowercase();
    let shown: Vec<_> = items.iter().filter(|(_, name)| needle.is_empty() || name.to_lowercase().contains(&needle)).cloned().collect();
    rsx! {
        div { class: "project-scope",
            button { class: "icon-button", r#type: "button", "aria-label": "{label}", "aria-haspopup": "listbox", "aria-expanded": "{open()}", title: "{label}",
                onclick: move |_| { query.set(String::new()); open.set(!open()); }, "\u{25a4}" }
            if open() {
                div { class: "menu-backdrop", onclick: move |_| open.set(false) }
                div { class: "context-menu project-scope-popup", role: "dialog", "aria-label": "Filter threads by project",
                    onkeydown: move |event| { if event.key() == Key::Escape { event.stop_propagation(); open.set(false); } },
                    input { "aria-label": "Search projects", placeholder: "Search projects...", value: "{query}",
                        onmounted: move |event| { drop(event.set_focus(true)); }, oninput: move |event| query.set(event.value()) }
                    div { role: "listbox", "aria-label": "Projects",
                        // "All projects" heads the list while the query is empty and drops out while filtering.
                        if needle.is_empty() {
                            button { class: "menu-item", role: "option", r#type: "button", "aria-selected": "{scoped.is_none()}", "aria-label": "All projects",
                                onclick: move |_| { set_scope(state, None); open.set(false); }, "All projects" }
                        }
                        for (key, name) in shown.iter().cloned() {
                            button { key: "{key}", class: "menu-item", role: "option", r#type: "button", "aria-selected": "{scoped.as_deref() == Some(key.as_str())}", "aria-label": "{name}",
                                onclick: { let key = key.clone(); move |_| { set_scope(state, Some(key.clone())); open.set(false); } }, "{name}" }
                        }
                        if shown.is_empty() { p { class: "muted", "No matching projects." } }
                    }
                }
            }
        }
    }
}

/// What `sidebarProjectGroupingOverrides` is keyed by for a directory of an environment.
pub(crate) fn override_key(environment: &str, workspace_root: &str) -> String {
    grouping::physical_key_of(environment, workspace_root)
}
