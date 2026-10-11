//! The per-turn changed-files card in the conversation, and the folder tree it shares with the
//! diff panel. The card is the main way into the diff: a file opens its turn at that file.
use crate::{
    diff_panel,
    runtime::{UiModel, UiModelStoreExt},
};
use dioxus::prelude::*;
use std::collections::BTreeMap;
use t3_client::{
    checkpoints::{ChangedFile, Scope},
    diff::{Stat, compact_count},
    diff_tree::{self, Node},
};

/// `+N -M`, with the original's compact counts.
#[component]
pub(crate) fn StatLabel(additions: u64, deletions: u64) -> Element {
    rsx! {
        span { class: "diff-stat", role: "group", "aria-label": "{additions} additions, {deletions} deletions",
            span { class: "additions", "aria-hidden": "true", "+{compact_count(additions)}" }
            span { class: "deletions", "aria-hidden": "true", "−{compact_count(deletions)}" }
        }
    }
}

struct TreeRow<'a> {
    depth: usize,
    node: &'a Node,
    open: bool,
}

/// The tree as the rows currently visible: a folder's children follow it only while it is open.
fn visible<'a>(
    nodes: &'a [Node],
    depth: usize,
    is_open: &dyn Fn(&str) -> bool,
    rows: &mut Vec<TreeRow<'a>>,
) {
    for node in nodes {
        let open = matches!(node, Node::Directory { path, .. } if is_open(path));
        rows.push(TreeRow { depth, node, open });
        if let (true, Node::Directory { children, .. }) = (open, node) {
            visible(children, depth + 1, is_open, rows);
        }
    }
}

/// Changed files grouped by folder. `expanded` is where every folder starts; a folder the user
/// toggled keeps its choice until the tree is remounted (the card keys it by that default).
#[component]
pub(crate) fn FileTree(
    files: Vec<ChangedFile>,
    expanded: bool,
    selected: Option<String>,
    /// Prefix of each file button's accessible name: "Show diff for", "Go to".
    action: String,
    label: String,
    on_select: Callback<String>,
    /// A right-click on a file: its path and the pointer (`useFileContextMenuHandler`).
    on_context: Option<Callback<(String, (f64, f64))>>,
) -> Element {
    let mut overrides = use_signal(BTreeMap::<String, bool>::new);
    let tree = use_memo(use_reactive((&files,), |(files,)| diff_tree::build(&files)));
    let tree = tree.read();
    let mut rows = Vec::new();
    visible(&tree, 0, &|path| overrides.read().get(path).copied().unwrap_or(expanded), &mut rows);
    let indented = rows.iter().any(|row| row.depth > 0) || rows.iter().any(|row| matches!(row.node, Node::Directory { .. }));
    rsx! {
        div { class: "file-tree", role: "group", "aria-label": "{label}",
            for row in rows {
                match row.node {
                    Node::Directory { name, path, stat, .. } => rsx! {
                        button { key: "dir:{path}", class: "tree-row tree-dir", style: "padding-left: {8 + row.depth * 14}px", "aria-expanded": "{row.open}", title: "{path}",
                            onclick: { let path = path.clone(); let open = row.open; move |_| { overrides.write().insert(path.clone(), !open); } },
                            span { class: "tree-chevron", "aria-hidden": "true", if row.open { "▾" } else { "▸" } }
                            span { class: "tree-name", "{name}" }
                            if stat.additions + stat.deletions > 0 { StatLabel { additions: stat.additions, deletions: stat.deletions } }
                        }
                    },
                    Node::File { name, path, stat } => rsx! {
                        button { key: "file:{path}", class: if selected.as_deref() == Some(path.as_str()) { "tree-row tree-file selected" } else { "tree-row tree-file" },
                            style: "padding-left: {8 + row.depth * 14}px", "aria-label": "{action} {path}", title: "{path}",
                            onclick: { let path = path.clone(); move |_| on_select.call(path.clone()) },
                            oncontextmenu: { let path = path.clone(); move |event| { if let Some(on_context) = on_context { event.prevent_default(); event.stop_propagation(); let at = event.client_coordinates(); on_context.call((path.clone(), (at.x, at.y))); } } },
                            if indented { span { class: "tree-chevron", "aria-hidden": "true" } }
                            span { class: "tree-name", "{name}" }
                            if let Some(Stat { additions, deletions }) = stat { StatLabel { additions: *additions, deletions: *deletions } }
                        }
                    },
                }
            }
        }
    }
}

/// What a finished turn changed, under the turn in the conversation. Shown only for a turn
/// that changed files, as the original does; a project without git has no checkpoint files.
#[component]
pub(crate) fn ChangedFiles(
    state: Store<UiModel>,
    environment_key: String,
    thread_id: String,
    item: serde_json::Value,
) -> Element {
    let files: Vec<ChangedFile> = item["files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(ChangedFile::from_value)
        .collect();
    if files.is_empty() {
        return rsx! {};
    }
    let run_id = item["runId"].as_str().map(str::to_owned);
    rsx! { Card { state, environment_key, thread_id, run_id, files } }
}

#[component]
fn Card(
    state: Store<UiModel>,
    environment_key: String,
    thread_id: String,
    run_id: Option<String>,
    files: Vec<ChangedFile>,
) -> Element {
    let mut all_expanded = use_signal(|| false);
    let can_open = diff_panel::use_can_read(state);
    // A subagent thread's checkpoints belong to that thread's own diff, not this panel's.
    let actionable = can_open() && run_id.is_some() && state.active_thread().peek().as_deref() == Some(thread_id.as_str());
    let summary = diff_tree::summarize(&files);
    let has_folders = files.iter().any(|file| file.path.contains(['/', '\\']));
    let first = files[0].path.clone();
    let open = use_callback(move |file: Option<String>| {
        if let Some(run_id) = run_id.clone() {
            diff_panel::open(state, &environment_key, &thread_id, Scope::Turn(run_id), file);
        }
    });
    let select = use_callback(move |path: String| open.call(Some(path)));
    let ignore = use_callback(|_: String| {});
    let action = if actionable { "Show diff for" } else { "Changed file" };
    let count = files.len();
    let noun = if count == 1 { "file" } else { "files" };
    rsx! {
        section { class: "changed-files", "data-changed-files-state": "tree", "aria-label": "Changed files",
            header {
                span { class: "changed-files-count", "{count} changed {noun}" }
                if summary.additions + summary.deletions > 0 { StatLabel { additions: summary.additions, deletions: summary.deletions } }
                span { class: "changed-files-actions",
                    if has_folders {
                        button { class: "tree-action", "aria-label": if all_expanded() { "Collapse all folders" } else { "Expand all folders" }, title: if all_expanded() { "Collapse all folders" } else { "Expand all folders" },
                            onclick: move |_| all_expanded.set(!all_expanded()), if all_expanded() { "Collapse all" } else { "Expand all" } }
                    }
                    if actionable {
                        button { class: "tree-action", "aria-label": "Open diff", title: "Open the full diff", onclick: move |_| open.call(Some(first.clone())), "Open diff" }
                    }
                }
            }
            FileTree { key: "{all_expanded()}", files: files.clone(), expanded: all_expanded(), selected: None, action, label: "Changed files", on_select: if actionable { select } else { ignore }, on_context: crate::link_menus::file_context() }
        }
    }
}
