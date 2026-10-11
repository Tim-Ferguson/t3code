//! A file of the open project, read by the server (`projects.readFile`) and shown in the panel
//! beside the conversation: what the file picker and the content search open, at the line a
//! content match names. Port of `FilePreviewPanel.tsx`: the file can be edited (a text field, for a file up to
//! `EDIT_LIMIT_BYTES`, when the connection may write files) and is saved by `t3_client::file_save` a moment after the edits stop.
//! Line comments: the number of a line is a button that opens a comment on it (Shift-click another for a range), which
//! joins the thread's message box as a chip. The file tree, tables, audio, video and PDF previews are in `file_tree`/`file_media`.
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

/// Opens a file as a tab of the panel beside the conversation, next to the files already open in the same place. Files of
/// another folder start a new row of tabs.
pub(crate) fn open(state: Store<UiModel>, owner: Option<String>, cwd: String, path: String, line: Option<u64>) {
    crate::file_tree::follow(state, &cwd);
    crate::right_panel::open_file(state, owner.as_deref(), &cwd, &path, line);
}
/// Closes the shown file's tab.
pub(crate) fn close(state: Store<UiModel>) {
    if matches!(crate::right_panel::shown_peek(state), Some(t3_client::right_panel::Surface::File { .. })) {
        crate::right_panel::close_shown(state);
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub(crate) fn key_of(cwd: &str, path: &str) -> String {
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

/// The project's files on their own, as the Files tab shows them.
#[component]
pub(crate) fn Explorer(state: Store<UiModel>, transport: TransportHandle, cwd: String, owner: Option<String>) -> Element {
    use_hook({
        let cwd = cwd.clone();
        move || crate::file_tree::follow(state, &cwd)
    });
    rsx! {
        section { class: "diff-panel files-panel", role: "region", "aria-label": "Files",
            header { class: "diff-header",
                div { class: "diff-header-main",
                    strong { "Files" }
                    button { class: "icon-button diff-close", "aria-label": "Close files", title: "Close the file list", onclick: move |_| crate::right_panel::close_kind(state, t3_client::right_panel::Kind::Files), "\u{00d7}" }
                }
            }
            aside { class: "file-explorer standalone", "aria-label": "Files",
                crate::file_tree::Tree { key: "{cwd}", state, transport, cwd: cwd.clone(), owner, selected: String::new() }
            }
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
pub(crate) fn Panel(state: Store<UiModel>, transport: TransportHandle, target: Target) -> Element {
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
    // The lines a comment is being written on, and the line a Shift-click extends from.
    let mut commenting = use_signal(|| None::<(u64, u64)>);
    let mut anchor = use_signal(|| None::<u64>);
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
    // A comment on lines goes to the open thread's message box as a chip (`buildFileReviewComment`).
    let comment_thread = target.owner.clone().filter(|_| !editing());
    let show_rendered = is_markdown && rendered() && !editing();
    let markdown_text = loaded.as_ref().filter(|_| show_rendered).map(|loaded| loaded.lines.join("\n")).unwrap_or_default();
    rsx! {
        section { class: "diff-panel file-panel", role: "region", "aria-label": "File {name}",
            header { class: "diff-header",
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
                    button { class: "icon-button diff-close", "aria-label": "Close file", title: "Close file", onclick: move |_| close(state), "\u{00d7}" }
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
                                        span { class: "file-line-number", role: "cell",
                                            if comment_thread.is_some() {
                                                button { class: "file-line-comment", r#type: "button", "aria-label": "Comment on line {number}", title: "Comment on this line. Shift-click another line to comment on a range.",
                                                    onclick: move |event| {
                                                        let line = number as u64;
                                                        let first = if event.modifiers().shift() { anchor.peek().unwrap_or(line) } else { line };
                                                        anchor.set(Some(line));
                                                        commenting.set(Some((first.min(line), first.max(line))));
                                                    },
                                                    "{number}" }
                                            } else { "{number}" }
                                        }
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
            if let (Some((first, last)), Some(thread_id)) = (commenting(), comment_thread.clone()) {
                FileCommentDialog { state, environment_key: environment_key.clone(), thread_id, path: target.path.clone(), first, last, contents: loaded.as_ref().map(|loaded| loaded.lines.join("\n")).unwrap_or_default(), onclose: move |()| commenting.set(None) }
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

/// Writes a comment on lines of the open file and attaches it to the thread's message box as a chip (`FilePreviewPanel.tsx`
/// annotations, `buildFileReviewComment`).
#[component]
fn FileCommentDialog(state: Store<UiModel>, environment_key: String, thread_id: String, path: String, first: u64, last: u64, contents: String, onclose: EventHandler<()>) -> Element {
    let mut text = use_signal(String::new);
    let title = if first == last { format!("Comment on {path} line {first}") } else { format!("Comment on {path} lines {first} to {last}") };
    let add = {
        let (path, contents, thread_id, environment_key) = (path.clone(), contents.clone(), thread_id.clone(), environment_key.clone());
        move || {
            let comment = t3_client::composer_context::file_comment(&format!("review-comment-{}", uuid::Uuid::new_v4().simple()), &path, first as u32, last as u32, &text.peek(), &contents);
            if comment["text"].as_str().is_some_and(str::is_empty) {
                return;
            }
            crate::composer_chips::add(state, t3_client::draft_storage::DraftTarget::thread(environment_key.clone(), thread_id.clone()), t3_client::composer_context::review_record(&comment));
            onclose.call(());
        }
    };
    let preview: String = contents.split('\n').skip(first.saturating_sub(1) as usize).take((last.max(first) - first + 1).min(12) as usize).collect::<Vec<_>>().join("\n");
    rsx! { div { class: "modal-backdrop", onclick: move |_| onclose.call(()),
        div { class: "dialog review-comment-dialog", role: "dialog", "aria-modal": "true", "aria-label": "{title}", tabindex: "-1", onclick: move |event| event.stop_propagation(),
            onkeydown: move |event| { if event.key() == Key::Escape { event.stop_propagation(); onclose.call(()); } },
            h2 { "{title}" }
            pre { class: "review-comment-line", "{preview}" }
            textarea { "aria-label": "File comment", placeholder: "What should change here?", value: "{text}", oninput: move |event| text.set(event.value()),
                onmounted: move |event| { let element = event.data(); spawn(async move { let _ = element.set_focus(true).await; }); } }
            div { class: "dialog-actions",
                button { r#type: "button", "aria-label": "Cancel file comment", onclick: move |_| onclose.call(()), "Cancel" }
                button { class: "primary", r#type: "button", "aria-label": "Add comment to chat", disabled: text.read().trim().is_empty(), onclick: { let mut add = add.clone(); move |_| add() }, "Add to chat" }
            }
        }
    } }
}
