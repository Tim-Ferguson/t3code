//! A file of the open project, read by the server (`projects.readFile`) and shown in the panel
//! beside the conversation: what the file picker and the content search open, at the line a
//! content match names. Port of `FilePreviewPanel.tsx`: the file can be edited (a text field, for a file up to
//! `EDIT_LIMIT_BYTES`, when the connection may write files) and is saved by `t3_client::file_save` a moment after the edits stop.
//! Not ported: the line comments, the file tree and the table, audio, video and PDF previews.
//!
//! The panel draws a window of the file, never all of it, so a file of tens of thousands of lines
//! costs the same as a short one: the window starts above the revealed line and grows by request.
use crate::{
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
    syntax_highlighting::{self, highlighted_line},
};
use dioxus::prelude::*;
use serde_json::json;
use std::{cell::RefCell, rc::Rc};
use t3_client::{diff::language_for_path, file_preview as logic_preview, file_save, project_files as files};
use t3_contracts::AuthEnvironmentScope;

/// Which file the panel shows, for which thread (or the new-thread form, when `owner` is None).
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub owner: Option<String>,
    pub cwd: String,
    /// Workspace-relative.
    pub path: String,
    pub line: Option<u64>,
    /// Grows with every opening, so choosing the same file again scrolls to the line again.
    pub request: u64,
}

/// Opens the panel on a file, as a tab beside the files already open in the same place; the diff and pull request
/// panels give way, since they share the margin. Files of another thread or folder start a new row of tabs.
pub(crate) fn open(state: Store<UiModel>, owner: Option<String>, cwd: String, path: String, line: Option<u64>) {
    state.diff().write().open = false;
    state.pr_panel().set(None);
    state.thread_prs().set(None);
    crate::file_tree::follow(state, &cwd);
    let request = state.file_panel().peek().as_ref().map_or(0, |target| target.request) + 1;
    {
        let mut tabs = state.file_tabs();
        let mut tabs = tabs.write();
        if tabs.first().is_some_and(|first| first.owner != owner || first.cwd != cwd) {
            tabs.clear();
        }
        if !tabs.iter().any(|tab| tab.path == path) {
            tabs.push(Target { owner: owner.clone(), cwd: cwd.clone(), path: path.clone(), line: None, request: 0 });
        }
    }
    state.file_panel().set(Some(Target { owner, cwd, path, line, request }));
}
/// Closes the panel and every tab.
pub(crate) fn close(state: Store<UiModel>) {
    state.file_tabs().set(Vec::new());
    state.file_panel().set(None);
}
/// Shows another open tab.
fn select(state: Store<UiModel>, path: &str) {
    let Some(current) = state.file_panel().peek().clone() else { return };
    if current.path != path {
        state.file_panel().set(Some(Target { path: path.to_owned(), line: None, request: current.request + 1, ..current }));
    }
}
/// The menu of a tab: copy its path, and close it, the others, the ones to its right or all.
async fn tab_menu(state: Store<UiModel>, path: String, at: (f64, f64)) {
    let paths: Vec<String> = state.file_tabs().peek().iter().map(|tab| tab.path.clone()).collect();
    let Some(index) = paths.iter().position(|open| *open == path) else { return };
    let Some(choice) = crate::context_menu::show("Tab actions", t3_client::context_menu::tab(true, index, paths.len()), Some(at)).await else { return };
    if choice == "copy-path" {
        crate::sidebar_rows::copy_text(&path);
        return;
    }
    // Tabs may have changed while the menu was open: the choice is applied to the tabs there are now.
    let paths: Vec<String> = state.file_tabs().peek().iter().map(|tab| tab.path.clone()).collect();
    let Some(index) = paths.iter().position(|open| *open == path) else { return };
    let Some(kept) = t3_client::context_menu::after_close(&choice, &paths, index) else { return };
    if kept.is_empty() {
        close(state);
        return;
    }
    let shown = state.file_panel().peek().as_ref().map(|target| target.path.clone());
    state.file_tabs().write().retain(|tab| kept.contains(&tab.path));
    if shown.as_ref().is_none_or(|shown| !kept.contains(shown)) {
        // The tab shown went with the choice: the one the menu was opened on takes its place, else the nearest left.
        select(state, if kept.contains(&path) { &path } else { kept.last().expect("not empty") });
    }
}
/// Closes one tab; the one beside it is shown, and the panel closes with the last.
fn close_tab(state: Store<UiModel>, path: &str) {
    let tabs = state.file_tabs().peek().clone();
    let Some(index) = tabs.iter().position(|tab| tab.path == path) else { return };
    state.file_tabs().write().remove(index);
    let remaining = state.file_tabs().peek().clone();
    let current = state.file_panel().peek().clone();
    if current.as_ref().is_none_or(|current| current.path != path) {
        return;
    }
    match remaining.get(index.min(remaining.len().saturating_sub(1))) {
        Some(next) => select(state, &next.path.clone()),
        None => state.file_panel().set(None),
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn key_of(cwd: &str, path: &str) -> String {
    format!("{cwd}\u{0}{path}")
}

/// What the saver of one panel needs to act on its effects. It outlives the panel's drawing: a write begun when the panel
/// closes still finishes (`spawn_forever`), and the signals it touches are written only while they exist.
#[derive(Clone)]
struct Saving {
    state: Store<UiModel>,
    transport: TransportHandle,
    cwd: String,
    path: String,
    saver: Rc<RefCell<file_save::Saver>>,
    /// Which timer is the one waiting: a replaced timer does nothing when it fires.
    ticket: Rc<std::cell::Cell<u64>>,
    read: Signal<Read>,
    /// False once the panel is gone: its signals are not touched after that.
    alive: Rc<std::cell::Cell<bool>>,
}
impl Saving {
    fn can_write(&self) -> bool {
        crate::settings_common::connected_allows(self.state, AuthEnvironmentScope::FilesystemWrite)
    }

    fn apply(&self, effects: Vec<file_save::Effect>) {
        let key = key_of(&self.cwd, &self.path);
        for effect in effects {
            match effect {
                file_save::Effect::Pending(pending) => {
                    let mut set = self.state.file_pending();
                    if pending { set.write().insert(key.clone()); } else { set.write().remove(&key); }
                }
                file_save::Effect::Schedule(milliseconds) => {
                    let ticket = self.ticket.get() + 1;
                    self.ticket.set(ticket);
                    let this = self.clone();
                    // Tied to the panel: a panel that closes writes at once instead (`dispose`).
                    spawn(async move {
                        runtime::delay(milliseconds).await;
                        if this.ticket.get() == ticket {
                            let effects = this.saver.borrow_mut().timer(this.can_write());
                            this.apply(effects);
                        }
                    });
                }
                file_save::Effect::Persist { revision, contents } => {
                    self.state.file_unsaved().write().insert(key.clone(), contents.clone());
                    let this = self.clone();
                    let (key, payload) = (key.clone(), json!({"cwd":self.cwd,"relativePath":self.path,"contents":contents}));
                    dioxus::dioxus_core::spawn_forever(async move {
                        let answer = runtime::request_value_reported(this.transport.clone(), this.state, files::WRITE_FILE, payload).await;
                        let succeeded = answer.is_ok();
                        if let Err(message) = answer {
                            crate::notice::push(this.state, crate::notice::Level::Error, "Could not save the file", message);
                        }
                        let effects = this.saver.borrow_mut().written(revision, succeeded, now_ms(), this.can_write());
                        let _ = key;
                        this.apply(effects);
                    });
                }
                file_save::Effect::Confirmed(contents) => {
                    let mut unsaved = self.state.file_unsaved();
                    if unsaved.peek().get(&key) == Some(&contents) {
                        unsaved.write().remove(&key);
                    }
                    // The view follows what is on disk, if the panel is still there.
                    if self.alive.get() {
                        let mut read = self.read;
                        *read.write() = Read::Ready(Rc::new(Loaded::of(&contents, contents.len() as u64, false)));
                    }
                }
            }
        }
    }
}

#[component]
pub(crate) fn Host(state: Store<UiModel>, transport: TransportHandle, thread_id: Option<String>) -> Element {
    let target = use_memo(move || state.file_panel().read().clone().filter(|target| target.owner == thread_id));
    // A list of one: a new key then makes a new panel, which reads its own file (a lone keyed child would be reused).
    rsx! {
        for target in target().into_iter() {
            Panel { key: "{target.cwd}:{target.path}:{target.request}", state, transport: transport.clone(), target }
        }
    }
}

struct Loaded {
    lines: Vec<String>,
    byte_length: u64,
    truncated: bool,
    /// The file as it is, for the editor to start from; empty for a file the panel will not edit.
    text: String,
}
impl Loaded {
    fn of(contents: &str, byte_length: u64, truncated: bool) -> Self {
        let text = if !truncated && byte_length <= files::EDIT_LIMIT_BYTES { contents.to_owned() } else { String::new() };
        Self { lines: files::lines(contents).into_iter().map(str::to_owned).collect(), byte_length, truncated, text }
    }
}
#[derive(Default)]
enum Read {
    #[default]
    Loading,
    Ready(Rc<Loaded>),
    Failed(String),
}

#[component]
fn Panel(state: Store<UiModel>, transport: TransportHandle, target: Target) -> Element {
    let mut read = use_signal(Read::default);
    let saving = use_hook({
        let (transport, cwd, path) = (transport.clone(), target.cwd.clone(), target.path.clone());
        move || Saving { state, transport, cwd, path, saver: Rc::new(RefCell::new(file_save::Saver::new(file_save::DEBOUNCE_MS))), ticket: Rc::new(std::cell::Cell::new(0)), read, alive: Rc::new(std::cell::Cell::new(true)) }
    });
    // Closing the panel (or switching to another file) writes what is not on disk yet.
    use_drop({
        let saving = saving.clone();
        move || {
            saving.alive.set(false);
            let effects = saving.saver.borrow_mut().dispose(saving.can_write());
            saving.apply(effects);
        }
    });
    let can_write = crate::settings_common::connected_allows(state, AuthEnvironmentScope::FilesystemWrite);
    let mut editing = use_signal(|| false);
    let mut edit_from = use_signal(String::new);
    let reveal = target.line;
    let mut start = use_signal(|| files::view_start(reveal));
    let mut end = use_signal(|| files::view_start(reveal) + files::VIEW_WINDOW);
    use_future({
        let (transport, cwd, path) = (transport.clone(), target.cwd.clone(), target.path.clone());
        move || {
            let skip = files::is_image_path(&path) || crate::file_media::Kind::of(&path).is_some();
            let (transport, cwd, path) = (transport.clone(), cwd.clone(), path.clone());
            async move {
                if skip {
                    return;
                }
                let owner = runtime::response_owner(&transport, state);
                let answer = runtime::request_value_reported(transport.clone(), state, files::READ_FILE, json!({"cwd":cwd,"relativePath":path})).await;
                // Another connection answered by now: not this panel's file.
                if owner.is_none() || owner != runtime::response_owner(&transport, state) {
                    return;
                }
                read.set(match answer {
                    Ok(value) => match value["contents"].as_str() {
                        Some(contents) => {
                            // An edit handed to the server a moment ago may not be on disk yet: it is what the file is.
                            let unsaved = state.file_unsaved().peek().get(&key_of(&cwd, &path)).cloned();
                            match unsaved {
                                Some(unsaved) => Read::Ready(Rc::new(Loaded::of(&unsaved, unsaved.len() as u64, false))),
                                None => Read::Ready(Rc::new(Loaded::of(contents, value["byteLength"].as_u64().unwrap_or(contents.len() as u64), value["truncated"].as_bool().unwrap_or(false)))),
                            }
                        }
                        None => Read::Failed("The server answered without file contents.".into()),
                    },
                    Err(message) => Read::Failed(files::read_failure_message(&message)),
                });
            }
        }
    });
    // Once the lines are drawn, the revealed one is brought to the middle.
    let ready = matches!(&*read.read(), Read::Ready(_));
    use_effect(move || {
        if let Some(line) = reveal.filter(|_| ready) {
            let _ = document::eval(&format!("document.getElementById('file-line-{line}')?.scrollIntoView({{block:'center'}});"));
        }
    });
    let (directory, name) = target.path.rsplit_once('/').map(|(directory, name)| (format!("{directory}/"), name.to_owned())).unwrap_or((String::new(), target.path.clone()));
    let language = language_for_path(&target.path);
    let loaded = match &*read.read() {
        Read::Ready(loaded) => Some(loaded.clone()),
        _ => None,
    };
    let total = loaded.as_ref().map_or(0, |loaded| loaded.lines.len());
    let (from, to) = ((*start.read()).min(total), (*end.read()).min(total));
    let window = loaded.as_ref().map(|loaded| loaded.lines[from..to].join("\n")).unwrap_or_default();
    let coloured = syntax_highlighting::use_lines(&window, &language);
    let tabs = state.file_tabs().read().clone();
    let is_image = t3_client::project_files::is_image_path(&target.path);
    let is_markdown = t3_client::project_files::is_markdown_path(&target.path);
    let media = crate::file_media::Kind::of(&target.path);
    let explorer = state.file_tree().read().open;
    // A table is a preference of this device, as the original keeps it; a revealed line only exists in the source.
    let table_delimiter = logic_preview::delimiter(&target.path);
    let mut table_preferred = use_signal(|| true);
    use_hook(move || spawn(async move {
        if let Ok(Some(stored)) = document::eval(&format!("try{{return window.localStorage.getItem('{}');}}catch{{return null;}}", logic_preview::RENDER_TABLE_KEY)).join::<Option<String>>().await {
            table_preferred.set(stored != "false");
        }
    }));
    let show_table = table_delimiter.is_some() && table_preferred() && reveal.is_none() && !editing();
    // The choice between the rendered page and its source is kept on this device, as the original keeps it.
    let mut rendered = use_signal(|| true);
    use_hook(move || spawn(async move {
        if let Ok(Some(stored)) = document::eval("try{return window.localStorage.getItem('t3code.renderMarkdown');}catch{return null;}").join::<Option<String>>().await {
            rendered.set(stored != "false");
        }
    }));
    let editor_busy = use_signal(|| false);
    let last = try_consume_context::<crate::open_in::Last>().map_or_else(|| use_signal(|| None::<String>), |last| last.0);
    let editor = crate::open_in::favorite(state, last.read().as_deref());
    let full_path = format!("{}/{}", target.cwd.trim_end_matches('/'), target.path);
    let environment_key = state.destination().read().as_ref().map(ToString::to_string).unwrap_or_default();
    let editable = can_write && !is_image && loaded.as_ref().is_some_and(|loaded| !loaded.truncated && loaded.byte_length <= files::EDIT_LIMIT_BYTES);
    let pending = state.file_pending().read().contains(&key_of(&target.cwd, &target.path));
    let show_rendered = is_markdown && rendered() && !editing();
    let markdown_text = loaded.as_ref().filter(|_| show_rendered).map(|loaded| loaded.lines.join("\n")).unwrap_or_default();
    rsx! {
        section { class: "diff-panel file-panel", role: "region", "aria-label": "File {name}",
            header { class: "diff-header",
                if tabs.len() > 1 {
                    div { class: "file-tabs", role: "tablist", "aria-label": "Open files",
                        for tab in tabs.iter() {
                            {
                                let tab_name = tab.path.rsplit('/').next().unwrap_or(&tab.path).to_owned();
                                let selected = tab.path == target.path;
                                let dirty = state.file_pending().read().contains(&key_of(&tab.cwd, &tab.path));
                                let (select_path, close_path) = (tab.path.clone(), tab.path.clone());
                                rsx! {
                                    span { key: "{tab.path}", class: if selected { "file-tab selected" } else { "file-tab" },
                                        oncontextmenu: { let path = tab.path.clone(); move |event| { event.prevent_default(); event.stop_propagation(); let at = event.client_coordinates(); let path = path.clone(); spawn(async move { tab_menu(state, path, (at.x, at.y)).await }); } },
                                        // The middle button closes a tab, as in the original.
                                        onauxclick: { let path = tab.path.clone(); move |event| { if event.trigger_button() == Some(dioxus::html::input_data::MouseButton::Auxiliary) { event.prevent_default(); event.stop_propagation(); close_tab(state, &path); } } },
                                        button { class: "file-tab-name", r#type: "button", role: "tab", "aria-selected": "{selected}", "aria-label": "{tab_name}", title: "{tab.path}", onclick: move |_| select(state, &select_path), "{tab_name}" if dirty { span { class: "file-tab-dirty", role: "img", "aria-label": "Unsaved changes", title: "Unsaved changes", " \u{2022}" } } }
                                        button { class: "file-tab-close", r#type: "button", "aria-label": "Close {tab_name}", onclick: move |_| close_tab(state, &close_path), "\u{00d7}" }
                                    }
                                }
                            }
                        }
                    }
                }
                div { class: "diff-header-main",
                    span { class: "file-panel-path", title: "{target.path}", span { class: "muted", "{directory}" } strong { "{name}" } }
                    if editable {
                        button { r#type: "button", "aria-label": if editing() { "Stop editing {name}" } else { "Edit {name}" }, "aria-pressed": "{editing()}",
                            onclick: {
                              let loaded = loaded.clone();
                              move |_| {
                                if !editing() {
                                    edit_from.set(loaded.as_ref().map(|loaded| loaded.text.clone()).unwrap_or_default());
                                }
                                let next = !editing();
                                editing.set(next);
                              }
                            },
                            if editing() { "Done" } else { "Edit" } }
                    }
                    if pending { span { class: "muted", role: "status", "Saving\u{2026}" } }
                    button { r#type: "button", "aria-label": if explorer { "Hide files" } else { "Show files" }, "aria-pressed": "{explorer}", title: "The files of this project",
                        onclick: move |_| { let next = !explorer; state.file_tree().write().open = next; },
                        if explorer { "Hide files" } else { "Files" } }
                    if table_delimiter.is_some() && reveal.is_none() {
                        button { r#type: "button", "aria-label": if table_preferred() { "Show table source" } else { "Show table" }, "aria-pressed": "{table_preferred()}",
                            onclick: move |_| { let next = !table_preferred(); table_preferred.set(next); document::eval(&format!("try{{window.localStorage.setItem('{}','{next}');}}catch{{}}", logic_preview::RENDER_TABLE_KEY)); },
                            if table_preferred() { "Show source" } else { "Show table" } }
                    }
                    if is_markdown {
                        button { r#type: "button", "aria-label": if rendered() { "Show markdown source" } else { "Show rendered markdown" }, "aria-pressed": "{rendered()}", onclick: move |_| { let next = !rendered(); rendered.set(next); document::eval(&format!("try{{window.localStorage.setItem('t3code.renderMarkdown','{next}');}}catch{{}}")); },
                            if rendered() { "Show markdown source" } else { "Show rendered markdown" } }
                    }
                    if let Some(editor) = editor {
                        {
                            let (transport, full_path) = (transport.clone(), full_path.clone());
                            rsx! { button { r#type: "button", "aria-label": "Open {name} in editor", title: "Open this file in an editor on the server's machine", disabled: editor_busy(), onclick: move |_| crate::open_in::open(&transport, state, editor_busy, last, full_path.clone(), editor.clone(), false), "Open in editor" } }
                        }
                    }
                    button { class: "icon-button diff-close", "aria-label": "Close file", title: "Close file", onclick: move |_| match tabs.len() { 0 | 1 => close(state), _ => close_tab(state, &target.path) }, "\u{00d7}" }
                }
            }
            div { class: "file-panel-body",
            div { class: "file-panel-main",
            if is_image {
                match target.owner.clone() {
                    Some(thread_id) => rsx! { div { class: "file-image", crate::file_preview::WorkspaceImage { key: "{target.path}", state, transport: transport.clone(), environment_key: environment_key.clone(), thread_id, path: target.path.clone() } } },
                    None => rsx! { p { class: "muted file-panel-note", role: "status", "Open a thread to preview this image." } },
                }
            } else if let Some(kind) = media {
                match target.owner.clone() {
                    Some(thread_id) => rsx! { crate::file_media::Media { key: "{target.path}", state, transport: transport.clone(), environment_key: environment_key.clone(), thread_id, cwd: target.cwd.clone(), path: target.path.clone(), kind } },
                    None => rsx! { p { class: "muted file-panel-note", role: "status", "Open a thread to preview this file." } },
                }
            } else {
            match &*read.read() {
                Read::Loading => rsx! { p { class: "muted file-panel-note", role: "status", "Loading file\u{2026}" } },
                Read::Failed(message) => rsx! { p { class: "error-banner", role: "alert", "{message}" } },
                Read::Ready(_) if editing() => rsx! {
                    textarea { class: "file-editor", "aria-label": "Edit {name}", spellcheck: false, value: "{edit_from}",
                        oninput: {
                            let saving = saving.clone();
                            move |event| {
                                // The text is the saver's; the field keeps its own, so typing repaints nothing else.
                                let effects = saving.saver.borrow_mut().change(event.value(), now_ms());
                                saving.apply(effects);
                            }
                        } }
                },
                Read::Ready(loaded) if show_table => rsx! {
                    if loaded.truncated { p { class: "muted file-panel-note", role: "status", "{logic_preview::limit_notice(loaded.byte_length)}" } }
                    crate::file_media::Table { name: target.path.clone(), text: loaded.text.clone(), delimiter: table_delimiter.unwrap_or(',') }
                },
                Read::Ready(_) if show_rendered => rsx! {
                    if loaded.as_ref().is_some_and(|loaded| loaded.truncated) {
                        p { class: "muted file-panel-note", role: "status", "{logic_preview::limit_notice(loaded.as_ref().map_or(0, |loaded| loaded.byte_length))}" }
                    }
                    div { class: "file-markdown", "aria-label": "Rendered {name}", crate::markdown::Markdown { text: markdown_text } }
                },
                Read::Ready(_) => rsx! {
                    if loaded.as_ref().is_some_and(|loaded| loaded.truncated) {
                        p { class: "muted file-panel-note", role: "status", "{logic_preview::limit_notice(loaded.as_ref().map_or(0, |loaded| loaded.byte_length))}" }
                    }
                    if total == 0 { p { class: "muted file-panel-note", role: "status", "This file is empty." } }
                    div { class: "file-lines", role: "table", "aria-label": "Contents of {name}",
                        if from > 0 {
                            button { class: "file-more", onclick: move |_| { let next = start.peek().saturating_sub(files::VIEW_STEP); start.set(next); }, "Show earlier lines" }
                        }
                        for (offset, text) in loaded.iter().flat_map(|loaded| loaded.lines[from..to].iter()).enumerate() {
                            {
                                let number = from + offset + 1;
                                let revealed = reveal == Some(number as u64);
                                let colour = coloured.read().as_ref().and_then(|lines| lines.get(offset).cloned());
                                rsx! {
                                    div { key: "{number}", id: "file-line-{number}", class: if revealed { "file-line revealed" } else { "file-line" }, role: "row", "aria-current": if revealed { "true" } else { "false" },
                                        span { class: "file-line-number", role: "cell", "{number}" }
                                        code { class: "file-line-text", role: "cell", if let Some(colour) = colour { highlighted_line { line: colour } } else { "{text}" } }
                                    }
                                }
                            }
                        }
                        if to < total {
                            button { class: "file-more", onclick: move |_| { let next = *end.peek() + files::VIEW_STEP; end.set(next); }, "Show more lines" }
                        }
                    }
                },
            }
            }
            }
            if explorer {
                aside { class: "file-explorer", "aria-label": "Files",
                    crate::file_tree::Tree { key: "{target.cwd}", state, transport: transport.clone(), cwd: target.cwd.clone(), owner: target.owner.clone(), selected: target.path.clone() }
                }
            }
            }
        }
    }
}
