//! The file tree beside the file panel: the project's folders, read one at a time as they are opened
//! (`projects.listEntries`), with a search by name, a refresh, "Expand all folders", and the menu of
//! a row. Port of `FileBrowserPanel.tsx` and `useDirectoryEntries.ts`; which rows are drawn is
//! `t3_client::file_tree`. The tree keeps what it has read while the panel moves between files, and
//! starts again when the panel is on another folder.
//!
//! Not ported: dragging a row into the composer (the composer is a text field here) and the tree's
//! icon set; a row shows its name, and a folder its fold arrow.
use crate::{
    notice::{self, Level},
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
};
use dioxus::prelude::*;
use std::collections::{BTreeMap, BTreeSet};
use t3_client::{
    context_menu::Item,
    file_tree::{self as logic, Entry},
    project_files::PATH_DEBOUNCE_MS,
};

/// Rows drawn before "Show more rows": a project can hold tens of thousands of entries once opened wide.
const ROW_LIMIT: usize = 1000;

/// What the tree has read of one folder of the machine.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Model {
    /// Whether the panel shows the tree beside the file; it stays so while the file changes.
    pub open: bool,
    pub cwd: String,
    /// Folder path (empty: the root) to its entries.
    pub directories: BTreeMap<String, Vec<Entry>>,
    /// Folders asked for, so a folder is asked for once; refreshing asks for them again.
    pub requested: BTreeSet<String>,
    pub expanded: BTreeSet<String>,
    pub errors: BTreeMap<String, String>,
    pub pending: usize,
    pub expand_all: bool,
}

/// Changes the model.
fn edit<R>(state: Store<UiModel>, change: impl FnOnce(&mut Model) -> R) -> R {
    let mut field = state.file_tree();
    let mut model = field.write();
    change(&mut model)
}
/// Reads the model without subscribing to it.
fn peek<R>(state: Store<UiModel>, read: impl FnOnce(&Model) -> R) -> R {
    let field = state.file_tree();
    let model = field.peek();
    read(&model)
}

/// Starts the tree over when the panel moves to another folder.
pub(crate) fn follow(state: Store<UiModel>, cwd: &str) {
    edit(state, |model| {
        if model.cwd != cwd {
            *model = Model { open: model.open, cwd: cwd.to_owned(), ..Model::default() };
        }
    });
}

/// Reads one folder. A folder already asked for is not asked again unless `force`.
fn load(state: Store<UiModel>, transport: &TransportHandle, cwd: &str, directory: &str, force: bool) {
    let started = edit(state, |model| {
        if model.cwd != cwd || (!force && model.requested.contains(directory)) {
            return false;
        }
        model.requested.insert(directory.to_owned());
        model.pending += 1;
        true
    });
    if !started {
        return;
    }
    let (transport, cwd, directory) = (transport.clone(), cwd.to_owned(), directory.to_owned());
    spawn(async move {
        let owner = runtime::response_owner(&transport, state);
        let answer = runtime::request_value(transport.clone(), state, logic::LIST_ENTRIES, logic::list_input(&cwd, &directory)).await;
        let current = owner.is_some() && owner == runtime::response_owner(&transport, state);
        edit(state, |model| {
            model.pending = model.pending.saturating_sub(1);
            // Another connection or folder answered by now: not this tree's.
            if model.cwd != cwd || !current {
                return;
            }
            match answer {
                Ok(value) => {
                    let entries = logic::children_of(&directory, value["entries"].as_array().map(Vec::as_slice).unwrap_or_default());
                    model.errors.remove(&directory);
                    model.directories.insert(directory, entries);
                }
                Err(message) => {
                    model.requested.remove(&directory);
                    model.errors.insert(directory, message);
                }
            }
        });
    });
}

/// Asks again for every folder visited, keeping what is open.
fn refresh(state: Store<UiModel>, transport: &TransportHandle, cwd: &str) {
    let visited: Vec<String> = peek(state, |model| model.requested.iter().cloned().collect());
    for directory in visited {
        load(state, transport, cwd, &directory, true);
    }
}

fn toggle_all(state: Store<UiModel>, transport: &TransportHandle, cwd: &str) {
    let open = peek(state, |model| !(model.expand_all || logic::all_expanded(&model.directories, &model.expanded)));
    edit(state, |model| {
        model.expand_all = open;
        if open {
            let known = logic::known_directories(&model.directories);
            model.expanded.extend(known);
        } else {
            model.expanded.clear();
        }
    });
    if open {
        load_unread(state, transport, cwd);
    }
}
/// Reads the open folders not read yet, and keeps open a folder that a row grew into.
fn load_unread(state: Store<UiModel>, transport: &TransportHandle, cwd: &str) {
    let unread: Vec<String> = peek(state, |model| logic::to_read(&model.directories, &model.expanded, &model.requested));
    for directory in unread {
        edit(state, |model| model.expanded.insert(directory.clone()));
        load(state, transport, cwd, &directory, false);
    }
}

fn add_to_chat(state: Store<UiModel>, path: &str) {
    let Some(active) = state.active_thread().peek().clone().filter(|_| state.destination().peek().is_some()) else {
        notice::push(state, Level::Error, "Unable to add to chat", "Open a chat for this project and try again.");
        return;
    };
    let _ = active;
    let prompt = state.draft().peek().clone();
    crate::draft_storage::edit_thread_prompt(state, logic::add_to_prompt(&prompt, path));
}

async fn row_menu(state: Store<UiModel>, transport: TransportHandle, cwd: String, path: String, at: (f64, f64)) {
    let absolute = t3_client::file_preview::absolute_path(&cwd, &path);
    let extra = vec![Item::new("copy-mention", "Copy mention"), Item::new("add-to-chat", "Add to chat")];
    match crate::link_menus::file(state, transport, Some(absolute), Some(at), extra).await.as_deref() {
        Some("copy-mention") => {
            crate::sidebar_rows::copy_text(&logic::mention(&path));
            notice::push(state, Level::Success, "Mention copied", path);
        }
        Some("add-to-chat") => add_to_chat(state, &path),
        _ => {}
    }
}

#[component]
pub(crate) fn Tree(state: Store<UiModel>, transport: TransportHandle, cwd: String, owner: Option<String>, selected: String) -> Element {
    let mut query = use_signal(String::new);
    let mut limit = use_signal(|| ROW_LIMIT);
    let project = cwd.rsplit('/').find(|part| !part.is_empty()).unwrap_or("project").to_owned();
    // The root, and the folders above the open file, are read and opened; asked again only when the file changes.
    use_effect(use_reactive((&selected, &cwd), {
        let transport = transport.clone();
        move |(selected, cwd)| {
            follow(state, &cwd);
            let unread = peek(state, |model| logic::unread_ancestors(&selected, &model.directories));
            edit(state, |model| model.expanded.extend(logic::ancestors(&selected)));
            for directory in unread {
                load(state, &transport, &cwd, &directory, false);
            }
            load(state, &transport, &cwd, "", false);
        }
    }));
    // "Expand all folders" keeps opening what it reaches, and a folder opened by hand is read.
    use_effect({
        let (transport, cwd) = (transport.clone(), cwd.clone());
        move || {
            let (expand_all, known) = {
                let field = state.file_tree();
                let model = field.read();
                (model.expand_all, logic::known_directories(&model.directories))
            };
            if expand_all && !peek(state, |model| known.is_subset(&model.expanded)) {
                edit(state, |model| model.expanded.extend(known));
            }
            load_unread(state, &transport, &cwd);
        }
    });
    let found = use_resource({
        let (transport, cwd) = (transport.clone(), cwd.clone());
        move || {
            let typed = query.read().trim().to_owned();
            let (transport, cwd) = (transport.clone(), cwd.clone());
            async move {
                if typed.is_empty() {
                    return None;
                }
                // Typing on cancels this run, so the server is asked once typing pauses.
                runtime::delay(PATH_DEBOUNCE_MS).await;
                let answer = runtime::request_value(transport, state, t3_client::project_files::SEARCH_ENTRIES, logic::search_input(&cwd, &typed)).await;
                Some(answer.map(|value| (value["entries"].as_array().map(|entries| entries.iter().filter_map(Entry::from_json).collect::<Vec<_>>()).unwrap_or_default(), value["truncated"] == true)))
            }
        }
    });
    let searching = !query.read().trim().is_empty();
    let search_state = found.read().clone().flatten();
    let model = state.file_tree().read().clone();
    let (rows, loading, failure, more_matches) = if searching {
        match &search_state {
            Some(Ok((matches, truncated))) => {
                let (directories, open) = logic::search_tree(matches);
                (logic::rows(&directories, &open), false, None, *truncated)
            }
            Some(Err(message)) => (vec![], false, Some(message.clone()), false),
            None => (vec![], true, None, false),
        }
    } else {
        let failure = model.errors.iter().find(|(path, _)| path.is_empty() || model.directories.contains_key(*path) || model.expanded.contains(*path)).map(|(_, message)| message.clone());
        (logic::rows(&model.directories, &model.expanded), model.pending > 0, failure, false)
    };
    let all_open = model.expand_all || logic::all_expanded(&model.directories, &model.expanded);
    let drawn = rows.len().min(limit());
    let hidden = rows.len() - drawn;
    let retry = {
        let (transport, cwd) = (transport.clone(), cwd.clone());
        move |_| {
            refresh(state, &transport, &cwd);
            load(state, &transport, &cwd, "", true);
        }
    };
    rsx! {
        div { class: "file-tree-panel",
            div { class: "file-tree-bar",
                button { class: "icon-button", r#type: "button", "aria-label": "Refresh workspace files", title: "Refresh files", "aria-busy": "{model.pending > 0}", onclick: retry.clone(), "\u{21bb}" }
                input { r#type: "search", name: "project-files-search", "aria-label": "Search {project} files", placeholder: "Search files", spellcheck: false, value: "{query}",
                    oninput: move |event| { limit.set(ROW_LIMIT); query.set(event.value()); },
                    onkeydown: move |event| { if event.key() == Key::Escape && !query.read().is_empty() { event.stop_propagation(); query.set(String::new()); } } }
                if !model.directories.is_empty() && !searching {
                    {
                        let (transport, cwd) = (transport.clone(), cwd.clone());
                        let label = if all_open { "Collapse all folders" } else { "Expand all folders" };
                        rsx! { button { class: "icon-button", r#type: "button", "aria-label": label, title: label, onclick: move |_| toggle_all(state, &transport, &cwd), if all_open { "\u{25b4}" } else { "\u{25be}" } } }
                    }
                }
            }
            if let Some(message) = failure { button { class: "error-banner file-tree-error", r#type: "button", onclick: retry.clone(), "{message} Click to retry." } }
            if more_matches { p { class: "muted file-panel-note", "More matches available. Refine your search." } }
            if loading { p { class: "muted file-panel-note", role: "status", "Loading files\u{2026}" } }
            if searching && !loading && rows.is_empty() && search_state.as_ref().is_some_and(Result::is_ok) { p { class: "muted file-panel-note", role: "status", "No matching files." } }
            div { class: "file-tree", role: "tree", "aria-label": "{project} files",
                for row in rows.iter().take(drawn) {
                    {
                        let row = row.clone();
                        let current = !row.directory && row.path == selected;
                        let (transport, cwd, owner) = (transport.clone(), cwd.clone(), owner.clone());
                        let (click_path, menu_path, chain, was_open) = (row.path.clone(), row.path.clone(), row.chain.clone(), row.expanded);
                        let (click_transport, menu_transport, click_cwd, menu_cwd) = (transport.clone(), transport.clone(), cwd.clone(), cwd.clone());
                        let directory = row.directory;
                        rsx! {
                            button { key: "{row.path}", class: if current { "file-tree-row selected" } else if row.ignored { "file-tree-row ignored" } else { "file-tree-row" },
                                r#type: "button", role: "treeitem", "aria-label": "{row.path}", "aria-level": "{row.depth + 1}", "aria-selected": "{current}", "aria-expanded": if directory { Some(row.expanded.to_string()) } else { None },
                                style: "padding-left: {row.depth * 14 + 8}px", title: "{row.path}",
                                onclick: move |_| {
                                    if directory {
                                        if searching { return; }
                                        edit(state, |model| {
                                            if was_open {
                                                for folder in &chain {
                                                    model.expanded.remove(folder);
                                                }
                                                model.expand_all = false;
                                            } else {
                                                model.expanded.extend(chain.iter().cloned());
                                            }
                                        });
                                        load(state, &click_transport, &click_cwd, &click_path, false);
                                    } else {
                                        crate::file_panel::open(state, owner.clone(), click_cwd.clone(), click_path.clone(), None);
                                    }
                                },
                                oncontextmenu: move |event| {
                                    event.prevent_default();
                                    event.stop_propagation();
                                    let at = event.client_coordinates();
                                    let (transport, cwd, path) = (menu_transport.clone(), menu_cwd.clone(), menu_path.clone());
                                    spawn(async move { row_menu(state, transport, cwd, path, (at.x, at.y)).await });
                                },
                                span { class: "file-tree-fold", "aria-hidden": "true", if directory { if row.expanded { "\u{25be}" } else { "\u{25b8}" } } }
                                span { class: "file-tree-name", "{row.label}" }
                            }
                        }
                    }
                }
                if hidden > 0 { button { class: "file-more", r#type: "button", onclick: move |_| { let next = limit() + ROW_LIMIT; limit.set(next); }, "Show {hidden.min(ROW_LIMIT)} more rows" } }
            }
        }
    }
}
