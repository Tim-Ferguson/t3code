//! The command palette: one box that runs a command, goes to a project or thread, changes the
//! theme, or finds a thread by its title or its messages. Port of `CommandPalette.tsx`, limited to
//! what this app has. Opening it, typing in it and moving through it are state of this component
//! alone, so the conversation, the sidebar and the composer are never repainted by it.
use crate::{
    keys::{self, Keys, Runner},
    project_create,
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt, View},
    search::{self, Excerpt},
    themes::Themes,
};
use dioxus::prelude::*;
use serde_json::json;
use t3_client::{
    palette::{self, ContentMatch, Extra, Group, Item, Kind, Page, Row},
    project_folder,
    source_control::{self as clone, Source},
    themes::{Appearance, Mode, storage::Action},
};
use t3_contracts::{AuthEnvironmentScope, SourceControlDiscoveryResult, SourceControlRepositoryInfo};

/// Where the palette opens: its first page, the theme submenu, or the name step of a new project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Intent {
    #[default]
    Root,
    Theme,
    NewProject,
    /// "Go to file" (`filePicker.toggle`) and "Search project contents" (`projectSearch.toggle`):
    /// the same overlay with another body.
    Files,
    Content,
}
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Model {
    pub open: bool,
    pub intent: Intent,
    /// Grows with every opening, so each one starts from a fresh query and page.
    pub opened: u64,
}

pub(crate) fn open(state: Store<UiModel>, intent: Intent) {
    let mut palette = state.palette();
    let mut model = palette.write();
    model.open = true;
    model.intent = intent;
    model.opened += 1;
}
pub(crate) fn close(state: Store<UiModel>) {
    if state.palette().peek().open {
        state.palette().write().open = false;
    }
}
/// `filePicker.toggle` and `projectSearch.toggle`: closes the overlay when it already shows that
/// body, shows it otherwise (`ToggleMode`).
pub(crate) fn toggle_mode(state: Store<UiModel>, intent: Intent) {
    let (open_now, current) = {
        let palette = state.palette();
        let model = palette.peek();
        (model.open, model.intent)
    };
    if open_now && current == intent {
        close(state);
    } else {
        open(state, intent);
    }
}
/// `commandPalette.toggle`: closes it when it is open, opens it on its first page otherwise.
pub(crate) fn toggle(state: Store<UiModel>) {
    if state.palette().peek().open {
        close(state);
    } else {
        open(state, Intent::Root);
    }
}

/// The header button, for devices with no keyboard to press Ctrl+K on.
#[component]
pub(crate) fn Button(state: Store<UiModel>) -> Element {
    let open_now = use_memo(move || state.palette().read().open);
    let hint = try_consume_context::<Keys>().and_then(|keys| keys.label("commandPalette.toggle"));
    rsx! {
        button { class: if open_now() { "icon-button palette-button selected" } else { "icon-button palette-button" }, "aria-label": "Command palette", "aria-pressed": "{open_now()}", "aria-haspopup": "dialog",
            title: match hint { Some(hint) => format!("Command palette ({hint})"), None => "Command palette".to_owned() },
            onclick: move |_| toggle(state), "\u{2315}" }
    }
}

#[component]
pub(crate) fn Host(state: Store<UiModel>, transport: TransportHandle) -> Element {
    let open_now = use_memo(move || state.palette().read().open);
    if !open_now() {
        return rsx! {};
    }
    let (intent, opened) = {
        let field = state.palette();
        let model = field.read();
        (model.intent, model.opened)
    };
    match intent {
        Intent::Files => rsx! { crate::project_search::FilePicker { key: "files:{opened}", state, transport } },
        Intent::Content => rsx! { crate::project_search::ContentSearch { key: "content:{opened}", state, transport } },
        _ => rsx! { Palette { key: "{opened}", state, transport, intent } },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Theme,
    Appearance,
    NewThreadIn,
    NewProject,
    /// Adding a project by cloning a repository; `CloneStep` says where in that it is.
    Clone,
}

/// The steps of cloning a repository into a new project: where from, which repository, where to.
#[derive(Debug, Clone, PartialEq)]
enum CloneStep {
    Sources,
    Repository(Source),
    Destination { remote_url: String, name: String },
}

fn relative(updated: &t3_contracts::UtcDateTime) -> String {
    t3_client::thread_status::ago_label(updated.0, chrono::Utc::now())
}

const RECENT_THREADS: usize = 12;
const SETTINGS: [(View, &str, &str, &[&str]); 12] = [
    (View::Projects, "Projects", "settings:projects", &["project", "name", "remove", "defaults", "overrides", "folder"]),
    (View::General, "General", "settings:general", &["auto-resume", "snooze", "usage limit", "limited threads", "settle", "auto-settle", "inactive", "merged"]),
    (View::Providers, "Providers", "settings:providers", &["provider", "model", "codex", "claude", "agents"]),
    (View::Appearance, "Appearance", "settings:appearance", &["theme", "font", "typography", "colors", "dark", "light"]),
    (View::Connections, "Connections", "settings:connections", &["server", "environment", "pair", "remote", "address"]),
    (View::Keybindings, "Keybindings", "settings:keybindings", &["shortcuts", "keys", "hotkeys", "bindings"]),
    (View::ScheduledTasks, "Scheduled tasks", "settings:scheduled-tasks", &["schedule", "automation", "cron", "webhook", "recurring", "interval", "tasks"]),
    (View::SourceControl, "Source control", "settings:source-control", &["git", "github", "gitlab", "bitbucket", "azure", "credentials", "token", "hosting", "accounts"]),
    (View::TextGeneration, "Text generation", "settings:text-generation", &["model", "commit message", "branch naming", "writing style", "titles", "pull request text", "branch prefix"]),
    (View::Storage, "Storage", "settings:storage", &["disk", "worktrees", "cleanup", "retention", "artifacts", "logs", "delete"]),
    (View::Archived, "Archived threads", "settings:archived", &["archive", "restore", "unarchive", "history"]),
    (View::Diagnostics, "Diagnostics", "settings:diagnostics", &["processes", "trace", "failures", "slow", "kill", "memory", "cpu", "logs"]),
];

#[component]
fn Palette(state: Store<UiModel>, transport: TransportHandle, intent: Intent) -> Element {
    let runner = use_context::<Runner>();
    let keys = runner.keys;
    let themes = try_consume_context::<Themes>();
    let add_project = try_consume_context::<crate::AddProject>();
    let mut query = use_signal(String::new);
    let mut screens = use_signal(move || match intent {
        Intent::Root => vec![],
        Intent::Theme => vec![Screen::Theme],
        Intent::NewProject => vec![Screen::NewProject],
        Intent::Files | Intent::Content => vec![],
    });
    let mut highlighted = use_signal(|| 0usize);
    let mut failure = use_signal(|| None::<String>);
    let mut creating = use_signal(|| false);
    let mut clone_step = use_signal(|| CloneStep::Sources);
    let mut clone_scan = use_signal(|| None::<SourceControlDiscoveryResult>);
    let screen = screens.read().last().copied();
    let typed = query.read().clone();
    let actions_only = typed.starts_with('>');
    // Threads are found by their messages from the first page only.
    let answer = search::use_search(state, transport.clone(), move || {
        let typed = query.read().clone();
        if screens.read().is_empty() && !typed.starts_with('>') { typed } else { String::new() }
    });

    let shell = state.shell().read().clone();
    let snapshot = shell.snapshot.clone();
    let projects = snapshot.as_ref().map(|snapshot| snapshot.projects.clone()).unwrap_or_default();
    let threads = snapshot.as_ref().map(|snapshot| snapshot.threads.clone()).unwrap_or_default();
    let active_id = state.active_thread().read().clone();
    let project_title = |id: &str| projects.iter().find(|project| project.id.as_str() == id).map(|project| project.title.to_string()).unwrap_or_default();
    let selected_project = {
        let selected = state.selected_project().read().clone();
        let active = threads.iter().find(|thread| Some(thread.id.as_str()) == active_id.as_deref()).map(|thread| thread.project_id.to_string());
        active.or(selected).filter(|id| projects.iter().any(|project| project.id.as_str() == id)).or_else(|| projects.first().map(|project| project.id.to_string()))
    };

    // ---- the items ----
    let item = |value: &str, title: &str, terms: &[&str], command: Option<&str>| -> Option<Item> {
        if let Some(command) = command {
            if !keys.can_run(command) {
                return None;
            }
        }
        let mut item = Item::action(value, title, terms);
        if let Some(command) = command {
            item.shortcut_command = Some(command.to_owned());
        }
        Some(item)
    };
    let project_order = state.client_settings().peek().sidebar_project_sort_order;
    let ordered_projects: Vec<_> = t3_client::thread_sort::sort_projects(&projects, &threads, project_order).into_iter().cloned().collect();
    let project_items: Vec<Item> = ordered_projects
        .iter()
        .map(|project| Item::action(&format!("project:{}", project.id), project.title.as_str(), &[project.title.as_str(), project.workspace_root.as_str()]).described(project.workspace_root.to_string()))
        .collect();
    // The original lists threads in the order the sidebar setting names, and shows when a person last wrote to one.
    let sort_order = state.client_settings().peek().sidebar_thread_sort_order;
    let thread_items: Vec<Item> = t3_client::thread_sort::sort_threads(threads.iter().filter(|thread| thread.deleted_at.is_none() && thread.archived_at.is_none()), sort_order)
        .into_iter()
        .map(|thread| {
            let project = project_title(thread.project_id.as_str());
            let mut parts = vec![project.clone()];
            if let Some(branch) = &thread.branch {
                parts.push(format!("#{branch}"));
            }
            if Some(thread.id.as_str()) == active_id.as_deref() {
                parts.push("Current thread".into());
            }
            let mut item = Item::action(&format!("thread:{}", thread.id), &thread.title, &[thread.title.as_str(), project.as_str(), thread.branch.as_ref().map_or("", |branch| branch.as_str()), thread.id.as_str()]).described(parts.join(" \u{b7} "));
            item.recency = Some(t3_client::thread_sort::sort_timestamp(thread, t3_contracts::SidebarThreadSortOrder::UpdatedAt));
            item.timestamp = Some(relative(&t3_client::thread_sort::shown_at(thread).into()));
            item
        })
        .collect();
    let recent: Vec<Item> = thread_items.iter().take(RECENT_THREADS).cloned().collect();

    let mut actions: Vec<Item> = vec![];
    if let Some(title) = selected_project.as_deref().map(project_title).filter(|title| !title.is_empty()) {
        if let Some(item) = item("action:new-thread", &format!("New thread in {title}"), &["new thread", "chat", "create", "draft"], Some("chat.new")) {
            actions.push(item);
        }
    }
    if !projects.is_empty() {
        let pick: Vec<Item> = ordered_projects.iter().map(|project| Item::action(&format!("new-thread-in:{}", project.id), project.title.as_str(), &[project.title.as_str(), project.workspace_root.as_str()]).described(project.workspace_root.to_string())).collect();
        actions.push(Item::submenu("action:new-thread-in", "New thread in...", &["new thread", "project", "pick", "choose", "select"], vec![Group::new("projects", "Projects", pick)]));
    }
    let entries: [(&str, &str, &[&str], &str); 20] = [
        ("action:open-file-picker", "Go to file", &["go to file", "open file", "file picker", "find file", "quick open"], "filePicker.toggle"),
        ("action:search-project-contents", "Search project contents", &["search project", "find in files", "grep", "content search", "text search"], "projectSearch.toggle"),
        ("action:new-thread-without-project", "New thread without a project", &["new thread", "no project", "without project", "none", "chat"], "chat.newWithoutProject"),
        ("action:toggle-diff", "Toggle diff panel", &["diff", "changes", "files changed", "review"], "diff.toggle"),
        ("action:open-in-editor", "Open in editor", &["editor", "open", "vscode", "cursor", "zed", "ide", "folder"], "editor.openFavorite"),
        ("action:toggle-terminal", "Toggle terminal", &["terminal", "shell", "console"], "terminal.toggle"),
        ("action:switch-branch", "Switch branch", &["branch", "git", "checkout", "worktree"], "composer.branch"),
        ("action:stop-turn", "Interrupt running turn", &["stop", "interrupt", "cancel", "abort"], "thread.stop"),
        ("action:next-thread", "Go to next thread", &["next thread", "forward", "down"], "thread.next"),
        ("action:previous-thread", "Go to previous thread", &["previous thread", "back", "up"], "thread.previous"),
        ("action:sidebar", "Toggle sidebar", &["sidebar", "threads list", "hide", "show"], "sidebar.toggle"),
        ("action:new-terminal", "New terminal", &["terminal", "shell", "console", "new terminal", "tab"], "terminal.new"),
        ("action:split-terminal", "Split terminal", &["terminal", "split", "pane", "beside"], "terminal.split"),
        ("action:split-terminal-vertical", "Split terminal vertically", &["terminal", "split", "pane", "below", "vertical"], "terminal.splitVertical"),
        ("action:close-terminal", "Close terminal", &["terminal", "close", "kill", "exit", "stop"], "terminal.close"),
        ("action:theme-editor", "Toggle theme editor", &["theme", "appearance", "colors", "palette", "customize"], "themeEditor.toggle"),
        ("action:choose-workspace", "Choose workspace", &["workspace", "worktree", "checkout", "new thread"], "composer.workspace"),
        ("action:previous-worktree", "Use previous worktree", &["previous worktree", "worktree", "workspace", "last"], "composer.previousWorktree"),
        ("action:undo-archive", "Undo archive", &["undo", "archive", "restore", "unarchive"], "thread.undo"),
        ("action:copy-thread-reference", "Copy PR link or thread ID", &["copy", "pull request", "pr link", "thread id", "reference"], "thread.copyReference"),
    ];
    for (value, title, terms, command) in entries {
        if let Some(item) = item(value, title, terms, Some(command)) {
            actions.push(item);
        }
    }
    if active_id.is_some() && *state.view().read() == View::Chat {
        actions.push(Item::action("action:focus-composer", "Focus composer", &["composer", "message", "type", "prompt", "input"]));
        if let Some(target) = state.pr_available().read().clone() {
            actions.push(Item::action("action:open-pull-request", &format!("Open pull request #{}", target.reference.number), &["pull request", "pr", "merge request", "mr", "review", "checks"]));
        }
        if allows_operate(state) {
            if let Some(thread) = threads.iter().find(|thread| Some(thread.id.as_str()) == active_id.as_deref()) {
                if thread.archived_at.is_none() && !crate::thread_lifecycle::archive_blocked(thread) {
                    actions.push(Item::action("action:archive-thread", "Archive thread", &["archive", "hide", "done", "thread"]));
                }
                actions.push(Item::action("action:delete-thread", "Delete thread", &["delete", "remove", "trash", "discard", "thread"]));
            }
        }
        if title_regeneration_available(state) {
            actions.push(Item::action("action:regenerate-title", "Regenerate thread title", &["title", "rename", "regenerate", "thread", "name"]));
        }
    }
    if let Some(pinned) = crate::sidebar_rows::open_thread_pinned(state) {
        if let Some(item) = item("action:toggle-pin", if pinned { "Unpin thread" } else { "Pin thread" }, &["pin", "unpin", "keep on top", "thread"], Some("thread.pin")) {
            actions.push(item);
        }
    }
    if allows_operate(state) {
        if let Some(settled) = crate::sidebar_rows::open_thread_settled(state) {
            if let Some(item) = item("action:toggle-settle", if settled { "Un-settle thread" } else { "Settle thread" }, &["settle", "un-settle", "unsettle", "done", "finish", "thread"], Some("thread.settle")) {
                actions.push(item);
            }
        }
    }
    if state.environments().read().records.len() > 0 && allows_operate(state) && project_create::new_projects_root(state).is_some() {
        actions.push(Item::action("action:new-project", "New project", &["new project", "create project", "empty", "repository", "repo", "git init", "name"]).keeping_open());
    }
    if allows_operate(state) {
        if let Some(title) = selected_project.as_deref().map(project_title).filter(|title| !title.is_empty()) {
            actions.push(Item::action("action:remove-project", &format!("Remove project {title}"), &["remove project", "delete project", "project"]));
        }
    }
    if allows_operate(state) && crate::new_thread::workspace_form_open(state) {
        actions.push(Item::action("action:checkout-pull-request", "Checkout pull request", &["pull request", "pr", "merge request", "mr", "checkout", "review", "worktree"]));
    }
    actions.push(Item::action("action:pull-requests", "Pull requests", &["pull request", "pr", "merge request", "mr", "review", "list"]));
    // The open thread's linked pull requests, and restarting its agent session.
    if let Some(thread) = active_id.as_deref().and_then(|id| threads.iter().find(|thread| thread.id.as_str() == id)).filter(|_| *state.view().read() == View::Chat) {
        let (linking, listing) = state.typed_config().read().as_ref().map(|config| (config.environment.capabilities.thread_pull_request_linking == Some(true), config.environment.capabilities.thread_pull_requests == Some(true))).unwrap_or_default();
        if linking || listing {
            actions.push(Item::action("action:link-pull-request", "Link pull request to thread", &["link", "pull request", "pr", "attach", "stack"]));
            if listing {
                let mut item = Item::action("action:open-thread-pull-requests", "Show linked pull requests", &["pull requests", "linked", "stack", "prs"]);
                item.disabled = serde_json::to_value(thread).map(|value| t3_client::thread_pull_requests::links(&value).is_empty()).unwrap_or(true);
                actions.push(item);
            }
        }
        if allows_operate(state) {
            actions.push(Item::action("action:restart-agent-session", "Restart agent session", &["restart", "reset", "reload", "agent", "session", "skills", "plugins", "mcp"]));
        }
    }
    if let Some(title) = selected_project.as_deref().map(project_title).filter(|title| !title.is_empty()) {
        actions.push(Item::action("action:project-settings", "Project settings", &["project", "settings", "name", "icon", "scripts", "model", "workspace", "grouping", "checkout", "remove", "t3.json"]).described(title));
    }
    if let Some(usage) = item("action:usage", "Open usage", &["usage", "use", "tokens", "cost", "spend", "limits", "stats", "analytics"], Some("usage.open")) {
        actions.push(usage);
    }
    if allows_operate(state) {
        actions.push(Item::action("action:import-projects", "Import projects", &["import", "projects", "history", "claude", "codex", "conversations", "sessions", "scan"]));
    }
    actions.push(Item::action("action:add-project", "Add project", &["add project", "folder", "directory", "browse", "existing", "path"]));
    if can_clone_now(state) {
        actions.push(Item::action("action:clone", "Clone repository", &["clone", "repository", "repo", "git", "github", "gitlab", "bitbucket", "azure", "url", "add project", "remote"]).keeping_open());
    }
    if let Some(themes) = &themes {
        let snapshot = themes.snapshot.read().clone();
        let catalog = themes.catalog.read().clone();
        let current = catalog.definition(catalog.half(&snapshot.theme, snapshot.theme_halves.as_ref(), snapshot.resolved_theme)).map(|theme| theme.id.clone());
        let mut theme_rows: Vec<Item> = vec![];
        let definitions = catalog.data.builtin.iter().chain(&catalog.custom).chain(catalog.environment.iter().filter(|theme| !catalog.custom.iter().any(|custom| custom.id == theme.id)));
        theme_rows.push({
            let mut row = Item::action("theme:system", "T3 Code (default)", &["T3 Code (default)", "theme", "appearance"]);
            if snapshot.theme == "system" {
                row.description = Some("Current".into());
            }
            row
        });
        for theme in definitions {
            let mut row = Item::action(&format!("theme:{}", theme.id), &theme.label, &[theme.label.as_str(), "theme", "appearance"]);
            if Some(&theme.id) == current.as_ref() && snapshot.theme != "system" {
                row.description = Some("Current".into());
            }
            theme_rows.push(row);
        }
        let mut change_theme = Item::submenu("action:change-theme", "Change theme", &["change theme", "appearance", "colors", "palette"], vec![Group::new("themes", "Change theme", theme_rows)]);
        change_theme.shortcut_command = Some("theme.select".into());
        actions.push(change_theme);
        let mode_rows: Vec<Item> = [(Mode::System, "System"), (Mode::Light, "Light"), (Mode::Dark, "Dark")]
            .into_iter()
            .map(|(mode, label)| {
                let mut row = Item::action(&format!("appearance:{}", mode.key()), label, &[label, "appearance", "mode"]);
                if snapshot.appearance_mode == mode {
                    row.description = Some("Current".into());
                }
                row
            })
            .collect();
        let mut change_appearance = Item::submenu("action:change-appearance", "Change appearance", &["change appearance", "light", "dark", "system", "mode", "toggle"], vec![Group::new("appearance", "Change appearance", mode_rows)]);
        change_appearance.shortcut_command = Some("appearance.cycle".into());
        actions.push(change_appearance);
    }
    actions.push(Item::action("action:settings", "Open settings", &["settings", "preferences", "configuration", "keybindings"]));
    let settings: Vec<Item> = SETTINGS.iter().map(|(_, label, value, terms)| Item::action(value, &format!("Settings: {label}"), &[&format!("settings {label}"), label, terms.join(" ").as_str()])).collect();

    let root = palette::root_groups(actions.into_iter().chain(settings).collect(), recent);
    let hits = if screens.read().is_empty() && !actions_only && !typed.trim().is_empty() { search::hits(state, &typed, &answer.read()) } else { vec![] };
    let search_threads: Vec<Item> = hits
        .iter()
        .filter_map(|hit| {
            let mut item = thread_items.iter().find(|item| item.value == format!("thread:{}", hit.thread.id)).cloned()?;
            item.content_match = hit.found.as_ref().map(|found| ContentMatch { user: found.user, snippet: found.snippet.clone() });
            // The server's excerpt joins the terms, so a message-only match survives the filter.
            if let Some(found) = &hit.found {
                item.search_terms.push(found.snippet.clone());
            }
            Some(item)
        })
        .collect();
    let page_groups: Vec<Group> = match screen {
        None => root,
        Some(Screen::NewThreadIn) => vec![Group::new("projects", "Projects", projects.iter().map(|project| Item::action(&format!("new-thread-in:{}", project.id), project.title.as_str(), &[project.title.as_str(), project.workspace_root.as_str()]).described(project.workspace_root.to_string())).collect())],
        Some(Screen::Theme) => root.iter().flat_map(|group| group.items.iter()).find(|item| item.value == "action:change-theme").map(|item| item.groups.clone()).unwrap_or_default(),
        Some(Screen::Appearance) => root.iter().flat_map(|group| group.items.iter()).find(|item| item.value == "action:change-appearance").map(|item| item.groups.clone()).unwrap_or_default(),
        Some(Screen::NewProject) => vec![],
        Some(Screen::Clone) => match &*clone_step.read() {
            CloneStep::Sources => {
                let readiness = clone::readiness(clone_scan.read().as_ref());
                let mut rows: Vec<Item> = [Source::Url].into_iter().chain(clone::sorted_providers(&readiness)).map(|source| {
                    let title = if source == Source::Url { "Git URL".to_owned() } else { format!("{} repository", source.label()) };
                    let ready = &readiness[&source];
                    let terms = ["clone", "remote", "repository", "repo", "git", source.label()];
                    let mut item = Item::action(&format!("clone-source:{}", source.value()), &title, &terms);
                    item.description = Some(if !ready.ready {
                        format!("Setup required. {}", ready.hint.clone().unwrap_or_default())
                    } else if source == Source::Url {
                        "Clone from a remote URL".to_owned()
                    } else {
                        format!("Clone {} {}", source.label(), source.path_hint())
                    });
                    item.disabled = !ready.ready;
                    item
                }).collect();
                rows.push(Item::action("action:clone-setup", "Open source control settings", &["setup", "required", "settings", "credentials", "sign in", "provider"]));
                vec![Group::new("sources", "Sources", rows)]
            }
            _ => vec![],
        },
    };
    // Each setting the catalog knows, by what it is called or does (only while searching).
    let setting_items: Vec<Item> = if screens.read().is_empty() && !actions_only && !typed.trim().is_empty() {
        crate::settings_common::find_settings(state, &typed).into_iter().map(|setting| {
            let section = t3_client::settings_search::section_label(&setting.to);
            let mut terms: Vec<&str> = vec![setting.title.as_str(), section];
            terms.extend(setting.search_terms.iter().map(String::as_str));
            let mut item = Item::action(&format!("setting:{}", setting.id), &setting.title, &terms).described(format!("Settings \u{b7} {section}"));
            item.secondary = setting.secondary;
            item
        }).collect()
    } else {
        vec![]
    };
    let extra = Extra { projects: project_items, settings: setting_items, threads: search_threads };
    let groups = palette::filter_groups(&page_groups, &typed, screen.is_some(), &extra);
    let (rows, values) = palette::build_rows(&groups);
    let count = values.len();
    let shown = (*highlighted.read()).min(count.saturating_sub(1));
    let highlighted_value = values.get(shown).cloned();
    let page = if screen.is_some() { Page::Submenu } else { Page::Root };

    let close_palette = move || close(state);
    let run = {
        let (transport, themes) = (transport.clone(), themes.clone());
        let runner = runner.clone();
        let (groups, page_groups) = (groups.clone(), page_groups.clone());
        move |value: String| {
            let Some(chosen) = palette::find_item(&groups, &value).or_else(|| palette::find_item(&page_groups, &value)).cloned() else { return };
            if chosen.kind == Kind::Submenu {
                let next = match value.as_str() {
                    "action:change-theme" => Screen::Theme,
                    "action:change-appearance" => Screen::Appearance,
                    _ => Screen::NewThreadIn,
                };
                screens.write().push(next);
                query.set(String::new());
                highlighted.set(0);
                return;
            }
            if matches!(value.as_str(), "action:open-file-picker" | "action:search-project-contents") {
                open(state, if value == "action:open-file-picker" { Intent::Files } else { Intent::Content });
                return;
            }
            if value == "action:new-project" {
                screens.write().push(Screen::NewProject);
                query.set(String::new());
                failure.set(None);
                return;
            }
            if value == "action:clone" {
                screens.write().push(Screen::Clone);
                clone_step.set(CloneStep::Sources);
                clone_scan.set(None);
                query.set(String::new());
                highlighted.set(0);
                failure.set(None);
                // Which providers are ready decides which sources are offered.
                let (transport, owner) = (transport.clone(), runtime::response_owner(&transport, state));
                spawn(async move {
                    let result = runtime::request_value_reported(transport.clone(), state, clone::DISCOVER, json!({})).await;
                    if owner.is_none() || owner != runtime::response_owner(&transport, state) {
                        return;
                    }
                    if let Ok(result) = result.and_then(|value| clone::decode_discovery(&value)) {
                        clone_scan.set(Some(result));
                    }
                });
                return;
            }
            if let Some(source) = value.strip_prefix("clone-source:").and_then(Source::parse) {
                clone_step.set(CloneStep::Repository(source));
                query.set(String::new());
                highlighted.set(0);
                failure.set(None);
                return;
            }
            // Everything below ends the palette's work, so it closes first and the command runs
            // on the page that is left behind.
            close_palette();
            if let Some(command) = chosen.shortcut_command.as_deref().filter(|command| !matches!(*command, "theme.select" | "appearance.cycle")) {
                runner.execute(command);
                return;
            }
            match value.as_str() {
                "action:focus-composer" => keys::focus_composer(),
                "action:open-pull-request" => {
                    // Read first, then write: a guard held across `open` would block its writes.
                    let target = state.pr_available().peek().clone();
                    if let Some(target) = target {
                        crate::pull_request_panel::open(state, target);
                    }
                }
                "action:regenerate-title" => regenerate_title(&transport, state),
                "action:link-pull-request" => {
                    let open = state.active_thread().peek().clone();
                    if let Some(id) = open {
                        crate::thread_pull_requests_panel::open_link_dialog(state, &id);
                    }
                }
                "action:open-thread-pull-requests" => {
                    let open = state.active_thread().peek().clone();
                    if let Some(id) = open {
                        crate::thread_pull_requests_panel::open(state, &id);
                    }
                }
                "action:restart-agent-session" => crate::thread_lifecycle::restart_session(state, &transport),
                "action:project-settings" => {
                    if let Some(id) = selected_project.clone() {
                        crate::projects_page::open(state, &id);
                    }
                }
                "action:archive-thread" => {
                    let open = state.active_thread().peek().clone();
                    if let Some(id) = open {
                        crate::thread_lifecycle::request_archive(state, &transport, &id);
                    }
                }
                "action:delete-thread" => {
                    let open = state.active_thread().peek().clone();
                    if let Some(id) = open {
                        crate::thread_lifecycle::request_delete(state, &transport, &id);
                    }
                }
                "action:remove-project" => {
                    if let Some(id) = selected_project.clone() {
                        crate::thread_lifecycle::request_remove_project(state, &id);
                    }
                }
                "action:clone-setup" => open_settings(state, View::SourceControl),
                "action:pull-requests" => state.view().set(View::PullRequests),
                "action:usage" => crate::usage_page::open(state),
                "action:import-projects" => crate::import_projects::open(state),
                "action:checkout-pull-request" => {
                    if let Some((project, cwd)) = crate::new_thread::form_workspace(state) {
                        crate::pull_request_thread::open(state, &project, &cwd, "");
                    }
                }
                "action:add-project" => {
                    if let Some(open) = add_project {
                        let mut flag = open.0;
                        flag.set(true);
                    }
                }
                "action:settings" => open_settings(state, View::Providers),
                other => {
                    if let Some(view) = SETTINGS.iter().find(|(_, _, value, _)| *value == other).map(|(view, ..)| view.clone()) {
                        open_settings(state, view);
                    } else if let Some(setting) = other.strip_prefix("setting:").and_then(|id| t3_client::settings_search::items().iter().find(|item| item.id == id)) {
                        crate::settings_common::go_to_setting(state, setting);
                    } else if let Some(id) = other.strip_prefix("thread:") {
                        runtime::select_thread(&transport, state, id.to_owned());
                    } else if let Some(id) = other.strip_prefix("project:") {
                        // A project from the search opens its newest thread, else starts one (`openProjectFromSearch`).
                        let order = state.client_settings().peek().sidebar_thread_sort_order;
                        let latest = state.shell().peek().snapshot.as_ref().and_then(|snapshot| t3_client::thread_sort::latest_for_project(&snapshot.threads, id, order).map(|thread| thread.id.to_string()));
                        match latest {
                            Some(thread) => runtime::select_thread(&transport, state, thread),
                            None => runtime::new_thread(&transport, state, Some(id.to_owned())),
                        }
                    } else if let Some(id) = other.strip_prefix("new-thread-in:") {
                        runtime::new_thread(&transport, state, Some(id.to_owned()));
                    } else if let Some(id) = other.strip_prefix("theme:") {
                        if let Some(themes) = &themes {
                            themes.change(Action::Theme(id.to_owned()));
                        }
                    } else if let Some(mode) = other.strip_prefix("appearance:").and_then(Mode::parse) {
                        if let Some(themes) = &themes {
                            themes.change(Action::Mode(mode));
                        }
                    }
                }
            }
        }
    };
    let _ = Appearance::Light;

    // ---- the new project step ----
    let name = typed.trim().to_owned();
    let environment_root = project_create::new_projects_root(state);
    let folder = project_folder::new_project_folder_name(&name);
    let preview = environment_root.as_deref().map(|root| project_folder::path_preview(root, &name));
    let create = {
        let transport = transport.clone();
        let name = name.clone();
        move || {
            if name.is_empty() || *creating.peek() {
                return;
            }
            creating.set(true);
            failure.set(None);
            let (transport, name) = (transport.clone(), name.clone());
            spawn(async move {
                let owner = runtime::response_owner(&transport, state);
                let result = project_create::create_named(transport.clone(), state, &name).await;
                creating.set(false);
                if owner.is_none() || owner != runtime::response_owner(&transport, state) {
                    return;
                }
                match result {
                    Ok(created) => {
                        close(state);
                        project_create::finish_created(&transport, state, &name, &created);
                    }
                    Err(message) => failure.set(Some(message)),
                }
            });
        }
    };

    // ---- the clone steps ----
    // Typing the repository and then the destination are the two text steps; both use the box.
    let step = clone_step.read().clone();
    let cloning = screen == Some(Screen::Clone) && step != CloneStep::Sources;
    let can_clone = can_clone_now(state);
    let platform = state.typed_config().read().as_ref().map(|config| serde_json::to_value(config.environment.platform.os).ok().and_then(|os| os.as_str().map(str::to_owned)).unwrap_or_default()).unwrap_or_default();
    let parent = {
        let field = state.typed_config();
        let base = field.read().as_ref().map(|config| config.settings.add_project_base_directory.to_string()).unwrap_or_default();
        if base.trim().is_empty() { "~/".to_owned() } else { project_folder::directory(&base) }
    };
    let submit_clone = {
        let (transport, step, typed, platform, parent) = (transport.clone(), step.clone(), typed.clone(), platform, parent);
        move || {
            let entered = typed.trim().to_owned();
            if entered.is_empty() || *creating.peek() {
                return;
            }
            match step.clone() {
                CloneStep::Sources => {}
                CloneStep::Repository(source) => {
                    let parent = parent.clone();
                    let mut proceed = move |remote_url: String, name: String| {
                        clone_step.set(CloneStep::Destination { remote_url, name: name.clone() });
                        query.set(clone::clone_destination_path(&parent, &clone::clone_directory_name(&name)));
                        highlighted.set(0);
                    };
                    let Some(input) = clone::lookup_input(source, &entered) else {
                        // A pasted URL needs no lookup.
                        proceed(clone::normalize_pasted_clone_url(&entered), entered);
                        return;
                    };
                    creating.set(true);
                    failure.set(None);
                    let (transport, owner) = (transport.clone(), runtime::response_owner(&transport, state));
                    spawn(async move {
                        let result = runtime::request_value_reported(transport.clone(), state, clone::LOOKUP, input).await;
                        creating.set(false);
                        if owner.is_none() || owner != runtime::response_owner(&transport, state) {
                            return;
                        }
                        match result.and_then(|value| serde_json::from_value::<SourceControlRepositoryInfo>(value).map_err(|error| format!("Invalid repository: {error}"))) {
                            Ok(info) => proceed(clone::default_clone_url(&info), info.name_with_owner),
                            Err(message) => failure.set(Some(format!("Repository lookup failed. {message}"))),
                        }
                    });
                }
                CloneStep::Destination { remote_url, .. } => {
                    if !can_clone {
                        failure.set(Some("This connection needs permission to write source control and add projects.".into()));
                        return;
                    }
                    let destination = match clone::validate_destination(&entered, &platform) {
                        Ok(destination) => destination,
                        Err(message) => {
                            failure.set(Some(format!("Clone failed. {message}")));
                            return;
                        }
                    };
                    creating.set(true);
                    failure.set(None);
                    let (transport, owner) = (transport.clone(), runtime::response_owner(&transport, state));
                    // An environment that tracks clones creates the project and clones in the background:
                    // progress follows the person in the clone notices, and only a problem found before
                    // git runs comes back here. An older one clones and then adds the project.
                    let tracked = state.typed_config().peek().as_ref().is_some_and(|config| config.environment.capabilities.project_clone_tracking == Some(true));
                    spawn(async move {
                        if tracked {
                            let project_id = uuid::Uuid::new_v4().to_string();
                            let created_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
                            let input = t3_client::project_clone::start_input(&project_id, &clone::project_title(&destination), &created_at, &remote_url, &destination);
                            let result = runtime::request_value_reported(transport.clone(), state, t3_client::project_clone::START, input).await;
                            creating.set(false);
                            if owner.is_none() || owner != runtime::response_owner(&transport, state) {
                                return;
                            }
                            match result {
                                Ok(_) => {
                                    close(state);
                                    runtime::new_thread(&transport, state, Some(project_id));
                                }
                                Err(message) => failure.set(Some(format!("Clone failed. {message}"))),
                            }
                            return;
                        }
                        let result = runtime::request_value_reported(transport.clone(), state, clone::CLONE, clone::clone_input(&remote_url, &destination)).await;
                        // A clone is added as a project, as the original does once git is done.
                        let result = match result {
                            Ok(cloned) => {
                                let cwd = cloned["cwd"].as_str().map(str::to_owned).unwrap_or(destination.clone());
                                let project_id = uuid::Uuid::new_v4().to_string();
                                runtime::request_value_reported(transport.clone(), state, "projects.mutate", json!({"type":"project.create","commandId":uuid::Uuid::new_v4().to_string(),"projectId":project_id,"title":clone::project_title(&cwd),"workspaceRoot":cwd}))
                                    .await
                                    .map(|_| (project_id, cwd))
                            }
                            Err(message) => Err(message),
                        };
                        creating.set(false);
                        if owner.is_none() || owner != runtime::response_owner(&transport, state) {
                            return;
                        }
                        match result {
                            Ok((project_id, cwd)) => {
                                close(state);
                                crate::notice::push(state, crate::notice::Level::Success, format!("Cloned {}", clone::project_title(&cwd)), cwd);
                                runtime::new_thread(&transport, state, Some(project_id));
                            }
                            Err(message) => failure.set(Some(format!("Clone failed. {message}"))),
                        }
                    });
                }
            }
        }
    };
    let clone_status = match &step {
        CloneStep::Sources => String::new(),
        CloneStep::Repository(Source::Url) => "Paste a Git URL, or owner/repo for a public GitHub repository.".to_owned(),
        CloneStep::Repository(source) => format!("Name the repository as {}.", source.path_hint()),
        CloneStep::Destination { name, .. } => format!("Clone {name} into this path. Change it to put it somewhere else."),
    };
    let clone_label = if matches!(step, CloneStep::Repository(_)) { "Look up repository" } else { "Clone repository" };
    let placeholder = match (screen, &step) {
        (Some(Screen::NewProject), _) => "Project name".to_owned(),
        (Some(Screen::Clone), CloneStep::Repository(Source::Url)) => "Enter Git clone URL".to_owned(),
        (Some(Screen::Clone), CloneStep::Repository(source)) => format!("Enter {} repository ({})", source.label(), source.path_hint()),
        (Some(Screen::Clone), CloneStep::Destination { .. }) => "Clone destination path".to_owned(),
        _ => palette::placeholder(page).to_owned(),
    };
    let can_create = allows_operate(state) && environment_root.is_some();
    let hint_values = values.clone();
    rsx! {
        div { class: "modal-backdrop palette-backdrop", onclick: move |_| close_palette(),
            div { class: "palette", role: "dialog", "aria-modal": "true", "aria-label": "Command palette", onclick: move |event| event.stop_propagation(),
                div { class: "palette-input",
                    if screen.is_some() { span { class: "palette-crumb", match screen { Some(Screen::Theme) => "Change theme", Some(Screen::Appearance) => "Change appearance", Some(Screen::NewProject) => "New project", Some(Screen::Clone) => "Clone repository", _ => "New thread in" } } }
                    input { r#type: "text", role: "combobox", "aria-label": placeholder.clone(), "aria-expanded": "true", "aria-controls": "palette-results", "aria-autocomplete": "list", "aria-activedescendant": highlighted_value.as_ref().map(|value| format!("palette-item-{value}")), placeholder, autocomplete: "off", spellcheck: "false", value: "{typed}",
                        onmounted: move |event| { spawn(async move { let _ = event.set_focus(true).await; }); },
                        oninput: move |event| { query.set(event.value()); highlighted.set(0); failure.set(None); },
                        onkeydown: {
                            let (mut run, mut create, mut submit_clone) = (run.clone(), create.clone(), submit_clone.clone());
                            move |event| {
                                // An IME candidate is committed with these keys; they are part of typing.
                                if event.is_composing() { return; }
                                let key = event.key();
                                match key {
                                    Key::Escape => { event.prevent_default(); event.stop_propagation(); close_palette(); }
                                    Key::Backspace if query.peek().is_empty() && !screens.peek().is_empty() => {
                                        event.prevent_default();
                                        failure.set(None);
                                        highlighted.set(0);
                                        // Inside a clone, back goes one step before it leaves.
                                        if screens.peek().last() == Some(&Screen::Clone) && *clone_step.peek() != CloneStep::Sources { clone_step.set(CloneStep::Sources); } else { screens.write().pop(); }
                                    }
                                    Key::ArrowDown if count > 0 => { event.prevent_default(); highlighted.set((shown + 1) % count); }
                                    Key::ArrowUp if count > 0 => { event.prevent_default(); highlighted.set((shown + count - 1) % count); }
                                    Key::Enter => {
                                        event.prevent_default();
                                        if screen == Some(Screen::NewProject) { create(); }
                                        else if cloning { submit_clone(); }
                                        else if let Some(value) = hint_values.get(shown) { run(value.clone()); }
                                    }
                                    _ => {}
                                }
                            }
                        } }
                }
                if screen == Some(Screen::NewProject) {
                    div { class: "palette-new-project",
                        if !can_create { p { class: "muted", role: "status", "This connection cannot create projects." } }
                        else if name.is_empty() { p { class: "muted", role: "status", match &environment_root { Some(root) => format!("Goes in {root}"), None => "Name the project. It gets its own folder, a README and a first commit.".to_owned() } } }
                        else { p { class: "muted", role: "status", match &preview { Some(path) => format!("Creates {path}"), None => format!("Creates a folder named {folder}") } } }
                        if let Some(message) = failure.read().clone() { p { class: "error-banner", role: "alert", "{message}" } }
                        button { class: "primary", r#type: "button", disabled: name.is_empty() || creating() || !can_create, onclick: { let mut create = create.clone(); move |_| create() }, if creating() { "Creating\u{2026}" } else { "Create project" } }
                    }
                } else if cloning {
                    div { class: "palette-new-project",
                        p { class: "muted", role: "status", "{clone_status}" }
                        if matches!(step, CloneStep::Destination { .. }) && !can_clone { p { class: "muted", role: "status", "This connection needs permission to write source control and add projects." } }
                        if let Some(message) = failure.read().clone() { p { class: "error-banner", role: "alert", "{message}" } }
                        button { class: "primary", r#type: "button", "aria-label": clone_label, disabled: typed.trim().is_empty() || creating() || (matches!(step, CloneStep::Destination { .. }) && !can_clone), onclick: { let mut submit_clone = submit_clone.clone(); move |_| submit_clone() }, if creating() { "Working\u{2026}" } else { "{clone_label}" } }
                    }
                } else if rows.is_empty() {
                    p { class: "palette-empty", role: "status", "{palette::empty_message(actions_only)}" }
                } else {
                    ul { id: "palette-results", class: "palette-results", role: "listbox", "aria-label": "Results",
                        for row in rows {
                            match row {
                                Row::Label { key, label, .. } => rsx! { li { key: "{key}", class: "palette-group", role: "presentation", "{label}" } },
                                Row::Item { key, item, index } => {
                                    let value = item.value.clone();
                                    let active = highlighted_value.as_deref() == Some(item.value.as_str());
                                    let hint = item.shortcut_command.as_deref().and_then(|command| keys.label(command));
                                    rsx! { li { key: "{key}", role: "presentation",
                                        button { id: "palette-item-{item.value}", class: if active { "palette-row active" } else { "palette-row" }, r#type: "button", role: "option", tabindex: "-1", "aria-label": "{item.title}", "aria-selected": "{active}", disabled: item.disabled,
                                            onmousedown: move |event| event.prevent_default(),
                                            onmousemove: move |_| { if let Some(index) = index { if *highlighted.peek() != index { highlighted.set(index); } } },
                                            onclick: { let mut run = run.clone(); move |_| run(value.clone()) },
                                            span { class: "palette-text",
                                                span { class: "palette-title", "{item.title}" if item.timestamp.is_some() { span { class: "palette-time", "{item.timestamp.clone().unwrap_or_default()}" } } }
                                                if let Some(found) = item.content_match.clone() { Excerpt { user: found.user, snippet: found.snippet, query: typed.clone() } }
                                                if let Some(description) = item.description.clone() { span { class: "palette-description", "{description}" } }
                                            }
                                            if let Some(hint) = hint { kbd { class: "palette-shortcut", "{hint}" } }
                                            if item.kind == Kind::Submenu { span { class: "palette-chevron", "aria-hidden": "true", "\u{203a}" } }
                                        }
                                    } }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Cloning writes to source control and adds a project.
fn can_clone_now(state: Store<UiModel>) -> bool {
    let environments_field = state.environments();
    let environments = environments_field.read();
    state.destination().read().as_ref().is_some_and(|destination| environments.allows(destination, AuthEnvironmentScope::SourceControlWrite) && environments.allows(destination, AuthEnvironmentScope::OrchestrationOperate))
}

fn open_settings(state: Store<UiModel>, view: View) {
    state.view().set(view);
    state.sidebar_open().set(false);
}

/// Whether the open thread can have its title written again: the server supports it, the
/// connection may operate, and a request is not already running.
fn title_regeneration_available(state: Store<UiModel>) -> bool {
    let supported = state.typed_config().read().as_ref().is_some_and(|config| config.environment.capabilities.thread_title_regeneration == Some(true));
    let Some(destination) = state.destination().read().clone() else { return false };
    let id = state.active_thread().read().clone();
    supported
        && state.environments().read().allows(&destination, t3_contracts::AuthEnvironmentScope::OrchestrationOperate)
        && state.shell().read().snapshot.as_ref().is_some_and(|snapshot| snapshot.threads.iter().any(|thread| Some(thread.id.as_str()) == id.as_deref() && !matches!(thread.title_regeneration, Some(Some(_)))))
}

fn allows_operate(state: Store<UiModel>) -> bool {
    *state.status().read() == t3_client::connection::ConnectionStatus::Connected && state.destination().read().as_ref().is_some_and(|destination| state.environments().read().allows(destination, AuthEnvironmentScope::OrchestrationOperate))
}
fn regenerate_title(transport: &runtime::TransportHandle, state: Store<UiModel>) {
    if !title_regeneration_available(state) {
        return;
    }
    let active = state.active_thread().peek().clone();
    if let Some(id) = active {
        runtime::command(transport, state, "thread.metadata.update", serde_json::json!({"threadId":id,"regenerateTitle":true}));
    }
}
