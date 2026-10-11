//! Finding a file of the open project: "Go to file" by name and "Search project contents" by text,
//! both opened from the command palette or their shortcuts (`filePicker.toggle`,
//! `projectSearch.toggle`). Port of `ProjectFilePicker.tsx` and `ProjectContentSearchDialog.tsx`.
//!
//! Both ask the server after a short pause and drop the request of a query that has been replaced,
//! so typing in a project of any size sends one question per pause and draws one answer. The
//! picker draws at most 200 rows and the content search draws a window of a hundred and grows it
//! on request, so nothing here is proportional to the size of the project.
use crate::{
    command_palette::{self, Intent},
    file_panel,
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt, View},
    syntax_highlighting,
};
use dioxus::prelude::*;
use std::{cell::Cell, rc::Rc};
use t3_client::{
    connection::ConnectionStatus,
    diff::language_for_path,
    project_files::{self as files, FileMatch, Options},
};
use t3_contracts::AuthEnvironmentScope;

/// The workspace the dialogs search, and the thread whose panel shows what they open.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Target {
    pub cwd: String,
    pub title: String,
    pub owner: Option<String>,
}

/// The project behind the open thread, or behind the new-thread form (`useActiveProjectTarget`).
pub(crate) fn active_target(state: Store<UiModel>) -> Option<Target> {
    let field = state.shell();
    let shell = field.read();
    let snapshot = shell.snapshot.as_ref()?;
    if let Some(id) = state.active_thread().read().clone() {
        let thread = snapshot.threads.iter().find(|thread| thread.id.as_str() == id)?;
        let project = snapshot.projects.iter().find(|project| project.id == thread.project_id)?;
        let cwd = thread.worktree_path.as_ref().map(ToString::to_string).unwrap_or_else(|| project.workspace_root.to_string());
        return Some(Target { cwd, title: project.title.to_string(), owner: Some(id) }).filter(|target| !target.cwd.is_empty());
    }
    if *state.view().read() != View::Chat {
        return None;
    }
    let selected = state.selected_project().read().clone();
    let project = selected.and_then(|id| snapshot.projects.iter().find(|project| project.id.as_str() == id)).or_else(|| snapshot.projects.first())?;
    Some(Target { cwd: project.workspace_root.to_string(), title: project.title.to_string(), owner: None })
}

fn can_read(state: Store<UiModel>) -> bool {
    *state.status().read() == ConnectionStatus::Connected && state.destination().read().as_ref().is_some_and(|destination| state.environments().read().allows(destination, AuthEnvironmentScope::FilesystemRead))
}
#[cfg(test)]
thread_local! {
    /// Every query sent to the server by the picker and the content search, in order.
    pub(crate) static REQUESTS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}
const NO_ACCESS: &str = "This connection cannot search host files.";

fn open_file(state: Store<UiModel>, target: &Target, path: String, line: Option<u64>) {
    command_palette::close(state);
    file_panel::open(state, target.owner.clone(), target.cwd.clone(), path, line);
}
fn back_to_commands(state: Store<UiModel>) {
    command_palette::open(state, Intent::Root);
}
fn scroll_to(id: &str) {
    let _ = document::eval(&format!("document.getElementById('{id}')?.scrollIntoView({{block:'nearest'}});"));
}

/// The file picker, or its stand-in when there is no project to search.
#[component]
pub(crate) fn FilePicker(state: Store<UiModel>, transport: TransportHandle) -> Element {
    match active_target(state) {
        Some(target) => rsx! { OpenPicker { key: "{target.cwd}", state, transport, target } },
        None => rsx! { Empty { label: "File picker", placeholder: "Search files\u{2026}", state } },
    }
}
#[component]
pub(crate) fn ContentSearch(state: Store<UiModel>, transport: TransportHandle) -> Element {
    match active_target(state) {
        Some(target) => rsx! { OpenContent { key: "{target.cwd}", state, transport, target } },
        None => rsx! { Empty { label: "Search project contents", placeholder: "Search project contents\u{2026}", state } },
    }
}

#[component]
fn Empty(state: Store<UiModel>, label: String, placeholder: String) -> Element {
    rsx! {
        div { class: "modal-backdrop palette-backdrop", onclick: move |_| command_palette::close(state),
            div { class: "palette", role: "dialog", "aria-modal": "true", "aria-label": "{label}", onclick: move |event| event.stop_propagation(),
                div { class: "palette-input",
                    input { r#type: "text", disabled: true, "aria-label": "{placeholder}", placeholder: "{placeholder}", onmounted: move |event| { spawn(async move { let _ = event.set_focus(true).await; }); },
                        onkeydown: move |event| { if event.key() == Key::Escape { event.prevent_default(); back_to_commands(state); } } }
                }
                p { class: "palette-empty", role: "status", "Open a project to search its files." }
            }
        }
    }
}

/// What the server last said about one query of the picker.
struct Answered {
    query: String,
    rows: Result<Rc<Vec<FileMatch>>, String>,
}

#[component]
fn OpenPicker(state: Store<UiModel>, transport: TransportHandle, target: Target) -> Element {
    let mut query = use_signal(String::new);
    let mut highlighted = use_signal(|| 0usize);
    let mut answer = use_signal(|| None::<Answered>);
    let first = use_hook(|| Rc::new(Cell::new(true)));
    let allowed = use_memo(move || can_read(state));
    use_resource({
        let (transport, cwd) = (transport.clone(), target.cwd.clone());
        move || {
            let typed = query.read().trim().to_owned();
            let allowed = allowed();
            let (transport, cwd, first) = (transport.clone(), cwd.clone(), first.clone());
            async move {
                if !allowed {
                    return;
                }
                // The first question (the empty query, recent files) goes at once; later ones wait for a pause.
                if !first.replace(false) {
                    runtime::delay(files::PATH_DEBOUNCE_MS).await;
                }
                #[cfg(test)]
                REQUESTS.with(|requests| requests.borrow_mut().push(typed.clone()));
                let owner = runtime::response_owner(&transport, state);
                let result = runtime::request_value_reported(transport.clone(), state, files::SEARCH_ENTRIES, files::entries_input(&cwd, &typed)).await;
                // An answer for another connection is not this dialog's.
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
    let message = files::empty_message(&typed, error.as_deref(), pending && allowed());
    let marked_query = rows.first().map(|_| answer.read().as_ref().map(|answered| answered.query.clone()).unwrap_or_default()).unwrap_or_default();
    let count = rows.len();
    let shown = (*highlighted.read()).min(count.saturating_sub(1));
    let chosen = {
        let (rows, target) = (rows.clone(), target.clone());
        move |position: usize| {
            if let Some(row) = rows.get(position) {
                open_file(state, &target, row.path.clone(), None);
            }
        }
    };
    rsx! {
        div { class: "modal-backdrop palette-backdrop", onclick: move |_| command_palette::close(state),
            div { class: "palette palette-tall", role: "dialog", "aria-modal": "true", "aria-label": "File picker", "data-palette-mode": "files", onclick: move |event| event.stop_propagation(),
                div { class: "palette-input",
                    input { r#type: "text", role: "combobox", "aria-label": "Search files\u{2026}", "aria-expanded": "true", "aria-controls": "file-picker-results", "aria-autocomplete": "list", "aria-activedescendant": if count > 0 { Some(format!("file-row-{shown}")) } else { None }, placeholder: "Search files\u{2026}", autocomplete: "off", spellcheck: "false", value: "{typed}",
                        onmounted: move |event| { spawn(async move { let _ = event.set_focus(true).await; }); },
                        oninput: move |event| { query.set(event.value()); },
                        onkeydown: {
                            let chosen = chosen.clone();
                            move |event| {
                                if event.is_composing() { return; }
                                match event.key() {
                                    Key::Escape => { event.prevent_default(); event.stop_propagation(); back_to_commands(state); }
                                    Key::ArrowDown if count > 0 => { event.prevent_default(); let next = (shown + 1) % count; highlighted.set(next); scroll_to(&format!("file-row-{next}")); }
                                    Key::ArrowUp if count > 0 => { event.prevent_default(); let next = (shown + count - 1) % count; highlighted.set(next); scroll_to(&format!("file-row-{next}")); }
                                    Key::Enter => { event.prevent_default(); chosen(shown); }
                                    _ => {}
                                }
                            }
                        } }
                }
                if rows.is_empty() {
                    p { class: "palette-empty", role: "status", "{message}" }
                } else {
                    ul { id: "file-picker-results", class: "palette-results", role: "listbox", "aria-label": "Files",
                        li { class: "palette-group", role: "presentation", "{target.title}" }
                        for (position, row) in rows.iter().enumerate() {
                            li { key: "{row.path}", role: "presentation",
                                button { id: "file-row-{position}", class: if position == shown { "palette-row active" } else { "palette-row" }, r#type: "button", role: "option", tabindex: "-1", "aria-label": "{row.path}", "aria-selected": "{position == shown}",
                                    onmousedown: move |event| event.prevent_default(),
                                    onmousemove: move |_| { if *highlighted.peek() != position { highlighted.set(position); } },
                                    onclick: { let chosen = chosen.clone(); move |_| chosen(position) },
                                    span { class: "palette-text",
                                        span { class: "palette-title", span { Marked { text: row.name.clone(), indices: row.name_indices.clone(), active: !marked_query.trim().is_empty() } } }
                                        span { class: "palette-description", Marked { text: row.path.clone(), indices: row.path_indices.clone(), active: !marked_query.trim().is_empty() } }
                                    }
                                }
                            }
                        }
                    }
                }
                footer { class: "palette-footer", span { kbd { "\u{21b5}" } " Open file" } span { kbd { "Esc" } " Back" } }
            }
        }
    }
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

/// What the server last said about one content query and its options.
struct Found {
    query: String,
    options: Options,
    result: Result<Rc<files::ContentResult>, String>,
}

#[component]
fn OpenContent(state: Store<UiModel>, transport: TransportHandle, target: Target) -> Element {
    let mut query = use_signal(String::new);
    let mut options = use_signal(Options::default);
    let mut selected = use_signal(|| 0usize);
    let mut visible = use_signal(|| files::VISIBLE_WINDOW);
    let mut found = use_signal(|| None::<Found>);
    let allowed = use_memo(move || can_read(state));
    // The wait before asking is spent only when the text changed; flipping an option asks at once.
    let last_text = use_hook(|| Rc::new(std::cell::RefCell::new(String::new())));
    use_resource({
        let (transport, cwd) = (transport.clone(), target.cwd.clone());
        move || {
            let typed = query.read().clone();
            let chosen = *options.read();
            let allowed = allowed();
            let (transport, cwd, last_text) = (transport.clone(), cwd.clone(), last_text.clone());
            async move {
                if !allowed || !files::content_query_ready(&typed) {
                    return;
                }
                let changed = *last_text.borrow() != typed;
                *last_text.borrow_mut() = typed.clone();
                if changed {
                    runtime::delay(files::CONTENT_DEBOUNCE_MS).await;
                }
                #[cfg(test)]
                REQUESTS.with(|requests| requests.borrow_mut().push(typed.clone()));
                let owner = runtime::response_owner(&transport, state);
                let result = runtime::request_value_reported(transport.clone(), state, files::SEARCH_CONTENTS, files::contents_input(&cwd, &typed, chosen)).await;
                if owner.is_none() || owner != runtime::response_owner(&transport, state) {
                    return;
                }
                let result = result.and_then(|value| files::decode_contents(&value, chosen.regex)).map(Rc::new);
                selected.set(0);
                visible.set(files::VISIBLE_WINDOW);
                found.set(Some(Found { query: typed, options: chosen, result }));
            }
        }
    });

    let typed = query.read().clone();
    let chosen = *options.read();
    let ready = files::content_query_ready(&typed);
    let current = found.read().as_ref().is_some_and(|found| found.query == typed && found.options == chosen);
    let pending = allowed() && ready && !current;
    let (matches, truncated, invalid, error) = match &*found.read() {
        Some(Found { result: Ok(result), .. }) if ready => (result.clone(), result.truncated, result.invalid_regex && chosen.regex, None),
        Some(Found { result: Err(message), .. }) if ready && current => (Rc::new(files::ContentResult::default()), false, false, Some(message.clone())),
        _ => (Rc::new(files::ContentResult::default()), false, false, None),
    };
    let error = if allowed() { error } else { Some(NO_ACCESS.to_owned()) };
    let total = matches.matches.len();
    let can_open = allowed() && !pending && error.is_none();
    let shown_rows = (*visible.read()).min(total);
    let groups = files::group(&matches.matches, shown_rows);
    let active = (*selected.read()).min(total.saturating_sub(1));
    let show_status = allowed() && (ready || error.is_some());
    let open_match = {
        let (matches, target) = (matches.clone(), target.clone());
        move |position: usize| {
            if let Some(found) = matches.matches.get(position).filter(|_| can_open) {
                open_file(state, &target, found.path.clone(), Some(found.line));
            }
        }
    };
    rsx! {
        div { class: "modal-backdrop palette-backdrop", onclick: move |_| command_palette::close(state),
            div { class: "palette palette-tall palette-content", role: "dialog", "aria-modal": "true", "aria-label": "Search file contents in {target.title}", "data-palette-mode": "content", onclick: move |event| event.stop_propagation(),
                div { class: "palette-input",
                    input { r#type: "text", role: "combobox", "aria-label": "Search in {target.title}", "aria-expanded": "true", "aria-controls": "content-results", placeholder: "Search in {target.title}", autocomplete: "off", spellcheck: "false", disabled: !allowed(), value: "{typed}",
                        onmounted: move |event| { spawn(async move { let _ = event.set_focus(true).await; }); },
                        oninput: move |event| { query.set(event.value()); },
                        onkeydown: {
                            let open_match = open_match.clone();
                            move |event| {
                                if event.is_composing() { return; }
                                match event.key() {
                                    Key::Escape => { event.prevent_default(); event.stop_propagation(); back_to_commands(state); }
                                    Key::ArrowDown if total > 0 => {
                                        event.prevent_default();
                                        let next = (active + 1) % total;
                                        selected.set(next);
                                        if next >= *visible.peek() { visible.set(next + files::VISIBLE_WINDOW); }
                                        scroll_to(&format!("content-row-{next}"));
                                    }
                                    Key::ArrowUp if total > 0 => {
                                        event.prevent_default();
                                        let next = (active + total - 1) % total;
                                        selected.set(next);
                                        if next >= *visible.peek() { visible.set(next + files::VISIBLE_WINDOW); }
                                        scroll_to(&format!("content-row-{next}"));
                                    }
                                    // While a newer query is on its way the rows belong to the older one; opening one would jump somewhere nobody asked for.
                                    Key::Enter => { event.prevent_default(); open_match(active); }
                                    _ => {}
                                }
                            }
                        } }
                    div { class: "palette-toggles",
                        button { class: "palette-toggle", r#type: "button", "aria-label": "Match case", "aria-pressed": "{chosen.case_sensitive}", title: "Match case", disabled: !allowed(), onclick: move |_| flip(options, |options| &mut options.case_sensitive), "Aa" }
                        button { class: "palette-toggle", r#type: "button", "aria-label": "Match whole word", "aria-pressed": "{chosen.whole_word}", title: "Match whole word", disabled: !allowed(), onclick: move |_| flip(options, |options| &mut options.whole_word), u { "ab" } }
                        button { class: "palette-toggle", r#type: "button", "aria-label": "Use regular expression", "aria-pressed": "{chosen.regex}", title: "Use regular expression", disabled: !allowed(), onclick: move |_| flip(options, |options| &mut options.regex), ".*" }
                    }
                }
                if show_status {
                    div { class: "palette-status", role: "status",
                        if pending { "Searching\u{2026}" }
                        else if let Some(message) = error.clone() { span { class: "palette-error", "{message}" } }
                        else if invalid { span { class: "palette-error", "Invalid regular expression" } }
                        else { "{files::summary(&matches.matches, truncated)}" }
                    }
                }
                if total == 0 {
                    p { class: "palette-empty", role: "status",
                        if !allowed() { "{NO_ACCESS}" }
                        else if ready && !pending && error.is_none() { "No results found." }
                        else { "Type to search across your project." }
                    }
                } else {
                    div { id: "content-results", class: "content-results", role: "listbox", "aria-label": "Matches",
                        for group in groups.iter() {
                            section { key: "{group.path}", class: "content-group",
                                div { class: "content-group-head",
                                    strong { "{group.name}" }
                                    if !group.directory.is_empty() { span { class: "muted content-directory", "{group.directory}" } }
                                    span { class: "content-count", "{group.rows.len()}" }
                                }
                                for position in group.rows.iter().copied() {
                                    {
                                        let found = &matches.matches[position];
                                        rsx! {
                                            button { key: "{position}", id: "content-row-{position}", class: if position == active { "content-row active" } else { "content-row" }, r#type: "button", role: "option", "aria-label": "{found.path} line {found.line}", "aria-selected": "{position == active}", disabled: !can_open,
                                                onmouseenter: move |_| { if *selected.peek() != position { selected.set(position); } },
                                                onclick: { let open_match = open_match.clone(); move |_| open_match(position) },
                                                span { class: "content-line-number", "{found.line}" }
                                                span { class: "content-line-text", MarkedLine { text: found.text.clone(), path: found.path.clone(), ranges: found.ranges.clone(), index: position } }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        if total > shown_rows {
                            button { class: "file-more", onclick: move |_| { let next = *visible.peek() + files::VISIBLE_WINDOW; visible.set(next); }, "Show more results" }
                        }
                    }
                }
                footer { class: "palette-footer", span { kbd { "\u{21b5}" } " Open file" } span { kbd { "Esc" } " Back" } }
            }
        }
    }
}

fn flip(mut options: Signal<Options>, pick: fn(&mut Options) -> &mut bool) {
    let mut next = *options.peek();
    let flag = pick(&mut next);
    *flag = !*flag;
    options.set(next);
}

/// One matched line, in the colours of its language, with the matched text marked.
#[component]
fn MarkedLine(text: String, path: String, ranges: Vec<(usize, usize)>, index: usize) -> Element {
    // The row is drawn plain at once and coloured in a task of its own: the rows in view first, the rest after a pause, so a
    // page of a hundred matches never holds the page up, and a new query drops the work of the old one with its rows.
    let mut ready = use_signal(|| false);
    use_future(move || async move {
        runtime::delay(if index < 12 { 0 } else { 100 + 5 * index as u64 }).await;
        ready.set(true);
    });
    let language = language_for_path(&path);
    let coloured = syntax_highlighting::use_lines(if ready() { &text } else { "" }, &language);
    let pieces = syntax_highlighting::split_marked(coloured.read().as_ref().and_then(|lines| lines.first()), &text, &ranges);
    rsx! {
        span { class: "syntax-line",
            for (position, piece) in pieces.into_iter().enumerate() {
                if piece.marked { mark { key: "{position}", class: "search-mark", span { class: "syntax-token", style: "{piece.style}", "{piece.text}" } } }
                else { span { key: "{position}", class: "syntax-token", style: "{piece.style}", "{piece.text}" } }
            }
        }
    }
}
