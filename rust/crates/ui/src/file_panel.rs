//! A file of the open project, read by the server (`projects.readFile`) and shown in the panel
//! beside the conversation: what the file picker and the content search open, at the line a
//! content match names. Port of the read-only side of `FilePreviewPanel.tsx`; saving is not here.
//!
//! The panel draws a window of the file, never all of it, so a file of tens of thousands of lines
//! costs the same as a short one: the window starts above the revealed line and grows by request.
use crate::{
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
    syntax_highlighting::{self, highlighted_line},
};
use dioxus::prelude::*;
use serde_json::json;
use std::rc::Rc;
use t3_client::{diff::language_for_path, project_files as files};

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

/// Opens the panel on a file; the diff and pull request panels give way, since they share the margin.
pub(crate) fn open(state: Store<UiModel>, owner: Option<String>, cwd: String, path: String, line: Option<u64>) {
    state.diff().write().open = false;
    state.pr_panel().set(None);
    state.thread_prs().set(None);
    let request = state.file_panel().peek().as_ref().map_or(0, |target| target.request) + 1;
    state.file_panel().set(Some(Target { owner, cwd, path, line, request }));
}
pub(crate) fn close(state: Store<UiModel>) {
    state.file_panel().set(None);
}

#[component]
pub(crate) fn Host(state: Store<UiModel>, transport: TransportHandle, thread_id: Option<String>) -> Element {
    let target = use_memo(move || state.file_panel().read().clone().filter(|target| target.owner == thread_id));
    match target() {
        Some(target) => rsx! { Panel { key: "{target.cwd}:{target.path}:{target.request}", state, transport, target } },
        None => rsx! {},
    }
}

struct Loaded {
    lines: Vec<String>,
    byte_length: u64,
    truncated: bool,
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
    let reveal = target.line;
    let mut start = use_signal(|| files::view_start(reveal));
    let mut end = use_signal(|| files::view_start(reveal) + files::VIEW_WINDOW);
    use_future({
        let (transport, cwd, path) = (transport.clone(), target.cwd.clone(), target.path.clone());
        move || {
            let (transport, cwd, path) = (transport.clone(), cwd.clone(), path.clone());
            async move {
                let owner = runtime::response_owner(&transport, state);
                let answer = runtime::request_value_reported(transport.clone(), state, files::READ_FILE, json!({"cwd":cwd,"relativePath":path})).await;
                // Another connection answered by now: not this panel's file.
                if owner.is_none() || owner != runtime::response_owner(&transport, state) {
                    return;
                }
                read.set(match answer {
                    Ok(value) => match value["contents"].as_str() {
                        Some(contents) => Read::Ready(Rc::new(Loaded { lines: files::lines(contents).into_iter().map(str::to_owned).collect(), byte_length: value["byteLength"].as_u64().unwrap_or(contents.len() as u64), truncated: value["truncated"].as_bool().unwrap_or(false) })),
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
    rsx! {
        section { class: "diff-panel file-panel", role: "region", "aria-label": "File {name}",
            header { class: "diff-header",
                div { class: "diff-header-main",
                    span { class: "file-panel-path", title: "{target.path}", span { class: "muted", "{directory}" } strong { "{name}" } }
                    button { class: "icon-button diff-close", "aria-label": "Close file", title: "Close file", onclick: move |_| close(state), "\u{00d7}" }
                }
            }
            match &*read.read() {
                Read::Loading => rsx! { p { class: "muted file-panel-note", role: "status", "Loading file\u{2026}" } },
                Read::Failed(message) => rsx! { p { class: "error-banner", role: "alert", "{message}" } },
                Read::Ready(_) => rsx! {
                    if loaded.as_ref().is_some_and(|loaded| loaded.truncated) {
                        p { class: "muted file-panel-note", role: "status", "This file is large, so only the start of it ({loaded.as_ref().map_or(0, |loaded| loaded.byte_length)} bytes in all) is shown." }
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
}
