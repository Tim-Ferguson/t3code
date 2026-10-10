//! The pull request panel's Code tab: the patch from `POST /api/pull-requests/diff` drawn by the diff panel's own file
//! sections (collapsed files draw nothing, an open one a window of rows, unchanged lines read through
//! `pullRequests.diffFileContents`), the reader's viewed marks, which live at the host and are written one file at a time, and
//! the conversation on the code: review threads under their lines, and the line comments a review collects before it is sent.
//!
//! Read only while the tab is mounted. A newer revision (the panel's refresh counter, moved by the
//! server's refresh notice) reads the first page again; an answer for another pull request or
//! connection is dropped. Files past the first page are read when asked for.
use crate::{
    diff_panel::{ExpandHost, ExpandSource, FileSection, LineNotes, Mounts, Pending},
    notice::{self, Level},
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
};
use dioxus::prelude::*;
use serde_json::Value;
use std::{cell::RefCell, collections::BTreeSet, rc::Rc};
use t3_client::{
    diff::{self, FileDiff},
    pull_request::{self as pr, Reference},
    pull_request_line_comments as lines,
    review::SourceKind,
};
use t3_contracts::DiffLayout;

/// Files drawn before "Show more files": a big pull request builds a page at a time.
const FILES_PER_PAGE: usize = 100;
/// Files open when nothing is marked viewed; the rest start collapsed and draw nothing.
const OPEN_FILES: usize = 5;

#[derive(Debug, Clone, PartialEq, Default)]
struct Loaded {
    files: Vec<Rc<FileDiff>>,
    next_cursor: Option<String>,
    omitted: Vec<(String, i64, i64)>,
}
#[derive(Debug, Clone, PartialEq, Default)]
enum Patch {
    #[default]
    Loading,
    Ready(Rc<Loaded>),
    Failed(String),
}

fn decode(answer: &Value) -> Loaded {
    Loaded {
        files: diff::parse(answer["patch"].as_str().unwrap_or_default()).into_iter().map(Rc::new).collect(),
        next_cursor: answer["nextCursor"].as_str().map(str::to_owned),
        omitted: answer["omittedFileStats"].as_array().into_iter().flatten().filter_map(|stat| Some((stat["path"].as_str()?.to_owned(), stat["additions"].as_i64()?, stat["deletions"].as_i64()?))).collect(),
    }
}

#[component]
pub(crate) fn Files(state: Store<UiModel>, transport: TransportHandle, reference: Reference, revision: u32, readable: bool, writable: bool, why: Option<String>, viewed_store: Option<String>, threads: Vec<Value>, can_inline: bool, can_reply: bool, can_resolve: bool, on_changed: Callback<()>) -> Element {
    use_context_provider(Mounts::default);
    let pending = use_signal(|| None::<String>);
    use_context_provider(|| Pending(pending));
    use_context_provider({
        let transport = transport.clone();
        move || ExpandHost { state, transport }
    });
    let mut patch = use_signal(Patch::default);
    let has_store = viewed_store.is_some();
    let mut viewed = use_signal(BTreeSet::<String>::new);
    let mut viewed_revision = use_signal(|| 0u32);
    let mut toggling = use_signal(|| None::<String>);
    let mut more = use_signal(|| false);
    let mut toggled = use_signal(BTreeSet::<String>::new);
    let mut shown = use_signal(|| FILES_PER_PAGE);

    // ---- the conversation on the code ----
    // The line being commented on, until the comment is added to the review or dropped.
    let mut draft = use_signal(|| None::<Draft>);
    // What the notes under the lines are drawn from. The rows hold the notes closure from their first draw, so what changes
    // after it (the threads, the grants) travels in this cell, and `notes_revision` is what repaints the rows when it does.
    let shared = use_hook(|| Rc::new(RefCell::new(Shared::default())));
    let mut notes_revision = use_signal(|| 0u64);
    {
        let shared = shared.clone();
        use_effect(use_reactive((&threads, &can_reply, &can_resolve, &writable, &why), move |(threads, can_reply, can_resolve, writable, why)| {
            *shared.borrow_mut() = Shared { threads, can_reply, can_resolve, writable, why };
            let next = *notes_revision.peek() + 1;
            notes_revision.set(next);
        }));
    }
    let review_key = crate::pull_request_review::review_key(&reference);
    let begin = {
        let reference_for_files = patch;
        use_callback(move |(path, line, right): (String, diff::DiffLine, bool)| {
            let Some(position) = lines::position(&line, right) else { return };
            let old_path = match &*reference_for_files.peek() {
                Patch::Ready(loaded) => loaded.files.iter().find(|file| file.path() == path).and_then(|file| file.previous_path().map(str::to_owned)),
                _ => None,
            };
            draft.set(Some(Draft { path, old_path, position }));
        })
    };
    use_context_provider({
        let (transport, reference, shared, review_key) = (transport.clone(), reference.clone(), shared.clone(), review_key.clone());
        move || LineNotes {
            enabled: can_inline,
            begin,
            under: Rc::new(move |path: &str, line: &diff::DiffLine, which: Option<bool>| {
                // Reading these is what makes the row follow them.
                let _ = notes_revision();
                let here = |side: &str, number: u64| lines::is_on(side, number, line) && which.is_none_or(|right| (side == "right") == right);
                let notes = shared.borrow();
                let threads: Vec<Value> = notes.threads.iter().filter(|thread| thread["path"] == path && lines::thread_anchor(thread).is_some_and(|(side, number)| here(&side, number))).cloned().collect();
                let pending: Vec<lines::Pending> = state.pr_reviews().read().get(&review_key).map(|draft| draft.comments.iter().filter(|comment| comment.path == path && lines::anchor(&comment.position).is_some_and(|(side, number)| here(side, number))).cloned().collect()).unwrap_or_default();
                let writing = draft.read().clone().filter(|writing| writing.path == path && lines::anchor(&writing.position).is_some_and(|(side, number)| here(side, number)));
                if threads.is_empty() && pending.is_empty() && writing.is_none() {
                    return None;
                }
                let (reference, transport) = (reference.clone(), transport.clone());
                let (can_reply, can_resolve, writable, why) = (notes.can_reply, notes.can_resolve, notes.writable, notes.why.clone());
                drop(notes);
                let on_changed_threads = Callback::new(move |_: ()| on_changed.call(()));
                Some(rsx! {
                    div { class: "pr-line-notes", "data-path": "{path}",
                        if !threads.is_empty() { crate::pull_request_review::Threads { state, transport: transport.clone(), reference: reference.clone(), threads, can_reply, can_resolve, writable, why, on_changed: on_changed_threads } }
                        for comment in pending { crate::pull_request_review::PendingCard { key: "{comment.id}", state, key_of_review: review_key.clone(), comment } }
                        if let Some(writing) = writing {
                            {
                                let label = format!("{}:{}", writing.path, lines::anchor(&writing.position).map_or(0, |(_, number)| number));
                                let (key, writing_for_add) = (review_key.clone(), writing.clone());
                                rsx! { crate::pull_request_review::LineDraft { label,
                                    on_add: move |body: String| {
                                        state.pr_reviews().write().entry(key.clone()).or_default().add(&writing_for_add.path, writing_for_add.old_path.as_deref(), writing_for_add.position.clone(), &body);
                                        draft.set(None);
                                    },
                                    on_cancel: move |_| draft.set(None) } }
                            }
                        }
                    }
                })
            }),
        }
    });

    // The first page, read now, again at each new revision, and when the connection returns. The
    // revision and the grant are props, so they are dependencies by `use_reactive`: a copy captured
    // at mount would never start the read again.
    {
        let (transport, reference) = (transport.clone(), reference.clone());
        use_resource(use_reactive((&revision, &readable), move |(_revision, readable)| {
            let (transport, reference) = (transport.clone(), reference.clone());
            async move {
                if !readable {
                    return;
                }
                let answer = runtime::post_environment_json(&transport, state, pr::DIFF_PATH, &pr::diff_payload(&reference, None)).await;
                match answer {
                    Ok(answer) => {
                        let next = Rc::new(decode(&answer));
                        if *patch.peek() != Patch::Ready(next.clone()) {
                            patch.set(Patch::Ready(next));
                        }
                    }
                    Err(message) => {
                        if !matches!(&*patch.peek(), Patch::Ready(_)) {
                            patch.set(Patch::Failed(message));
                        }
                    }
                }
            }
        }));
    }
    // Which files the reader has marked viewed; marking one reads this again.
    {
        let (transport, reference) = (transport.clone(), reference.clone());
        use_resource(use_reactive((&revision, &readable, &has_store), move |(_revision, readable, kept)| {
            let (transport, reference, _viewed) = (transport.clone(), reference.clone(), viewed_revision());
            async move {
                // A host that keeps no marks has none to read, and offers no checkbox.
                if !readable || !kept {
                    return;
                }
                let owner = runtime::response_owner(&transport, state);
                let answer = runtime::request_value_reported(transport.clone(), state, pr::FILES_VIEWED, reference.payload(serde_json::json!({}))).await;
                if owner.is_none() || owner != runtime::response_owner(&transport, state) {
                    return;
                }
                if let Ok(answer) = answer {
                    let next = pr::viewed_paths(&answer);
                    if *viewed.peek() != next {
                        viewed.set(next);
                    }
                }
            }
        }));
    }
    let load_more = {
        let (transport, reference) = (transport.clone(), reference.clone());
        use_callback(move |cursor: String| {
            if *more.peek() {
                return;
            }
            more.set(true);
            let (transport, reference) = (transport.clone(), reference.clone());
            spawn(async move {
                let answer = runtime::post_environment_json(&transport, state, pr::DIFF_PATH, &pr::diff_payload(&reference, Some(&cursor))).await;
                match answer {
                    Ok(answer) => {
                        let page = decode(&answer);
                        let current = patch.peek().clone();
                        if let Patch::Ready(current) = current {
                            if current.next_cursor.as_deref() == Some(cursor.as_str()) {
                                let mut files = current.files.clone();
                                files.extend(page.files);
                                patch.set(Patch::Ready(Rc::new(Loaded { files, next_cursor: page.next_cursor, omitted: page.omitted })));
                            }
                        }
                    }
                    Err(message) => notice::push(state, Level::Error, "Could not load more files", message),
                }
                more.set(false);
            });
        })
    };
    let mark = {
        let (transport, reference) = (transport.clone(), reference.clone());
        use_callback(move |(path, mark): (String, bool)| {
            if toggling.peek().is_some() || !writable {
                return;
            }
            toggling.set(Some(path.clone()));
            let (transport, reference) = (transport.clone(), reference.clone());
            spawn(async move {
                let owner = runtime::response_owner(&transport, state);
                let answer = runtime::request_value_reported(transport.clone(), state, pr::SET_FILES_VIEWED, pr::set_viewed_payload(&reference, &path, mark)).await;
                if owner.is_some() && owner == runtime::response_owner(&transport, state) {
                    match answer {
                        Ok(_) => viewed_revision += 1,
                        Err(message) => notice::push(state, Level::Error, "Could not update the viewed mark", pr::readable_failure(&message, "The host refused it.")),
                    }
                }
                toggling.set(None);
            });
        })
    };
    let on_toggle = use_callback(move |path: String| {
        let mut set = toggled.write();
        if !set.remove(&path) {
            set.insert(path);
        }
    });

    let split = state.client_settings().read().diff_layout == DiffLayout::Split;
    let expand = Rc::new(ExpandSource { cwd: String::new(), kind: SourceKind::BranchRange, base_ref: None, head_ref: None, pull_request: Some(reference.clone()) });
    match patch.read().clone() {
        Patch::Loading => rsx! { p { class: "diff-message", role: "status", if readable { "Loading files\u{2026}" } else { "Reconnect to load the files." } } },
        Patch::Failed(message) => rsx! { p { class: "diff-error", role: "alert", "{message}" } },
        Patch::Ready(loaded) => {
            let viewed_now = viewed.read().clone();
            let total = loaded.files.len();
            let drawn = shown().min(total);
            let busy = toggling.read().clone();
            rsx! {
                section { class: "pr-files", "aria-label": "Files changed",
                    p { class: "pr-files-summary muted", role: "status", "{total} files in this page" if loaded.next_cursor.is_some() { " (more to load)" }
                        if let Some(store) = viewed_store.clone().filter(|_| total > 0) {
                            " \u{b7} " span { "aria-label": "Files viewed", "{pr::viewed_summary(loaded.files.iter().filter(|file| viewed_now.contains(file.path())).count(), total, &store)}" }
                            if store == "environment" { " " span { class: "pr-viewed-note", role: "img", "aria-label": "These ticks are kept here, not on the host", title: "{pr::VIEWED_ENVIRONMENT_NOTE}", "\u{24d8}" } }
                        }
                    }
                    for (index, file) in loaded.files.iter().take(drawn).enumerate() {
                        {
                            let path = file.path().to_owned();
                            let is_viewed = viewed_now.contains(&path);
                            let opened = toggled.read().contains(&path);
                            let collapsed = if is_viewed || index >= OPEN_FILES { !opened } else { opened };
                            let mark_path = path.clone();
                            rsx! {
                                div { key: "{path}", class: "pr-file",
                                    if has_store { label { class: "pr-viewed", title: why.clone().unwrap_or_default(),
                                        input { r#type: "checkbox", "aria-label": "Viewed {path}", checked: is_viewed, disabled: !writable || busy.is_some(),
                                            onclick: move |event| { event.prevent_default(); mark.call((mark_path.clone(), !is_viewed)); } }
                                        " Viewed"
                                    } }
                                    FileSection { file: file.clone(), collapsed, split, on_toggle, stat: None, partial: false, expand: Some(expand.clone()) }
                                    {
                                        // A conversation the diff no longer shows a line for (outdated, or on a line outside every hunk) is drawn under its file.
                                        let here: Vec<Value> = threads.iter().filter(|thread| thread["path"] == path.as_str() && !on_a_line(file, thread)).cloned().collect();
                                        rsx! { if !here.is_empty() { crate::pull_request_review::Threads { state, transport: transport.clone(), reference: reference.clone(), threads: here, can_reply, can_resolve, writable, why: why.clone(), on_changed } } }
                                    }
                                }
                            }
                        }
                    }
                    {
                        let strays: Vec<Value> = threads.iter().filter(|thread| !loaded.files.iter().any(|file| thread["path"] == file.path())).cloned().collect();
                        rsx! { if !strays.is_empty() { div { class: "pr-stray-threads", h4 { "Conversations on files not shown" }
                            crate::pull_request_review::Threads { state, transport: transport.clone(), reference: reference.clone(), threads: strays, can_reply, can_resolve, writable, why: why.clone(), on_changed } } } }
                    }
                    if drawn < total { button { "aria-label": "Show more files", onclick: move |_| shown.set(drawn + FILES_PER_PAGE), "Show {(total - drawn).min(FILES_PER_PAGE)} more files ({total - drawn} not shown)" } }
                    if let Some(cursor) = loaded.next_cursor.clone() {
                        button { "aria-label": "Load more files from the host", disabled: *more.read(), onclick: move |_| load_more.call(cursor.clone()), if *more.read() { "Loading more files\u{2026}" } else { "Load more files" } }
                    }
                    if !loaded.omitted.is_empty() {
                        div { class: "pr-omitted", h4 { "Files too large to show" }
                            ul { for (path, additions, deletions) in loaded.omitted.iter() { li { key: "{path}", "{path} +{additions} \u{2212}{deletions}" } } }
                        }
                    }
                }
            }
        }
    }
}

/// The line being commented on.
#[derive(Debug, Clone, PartialEq)]
struct Draft {
    path: String,
    old_path: Option<String>,
    position: Value,
}
/// What the notes under the lines read, which changes after the rows are drawn.
#[derive(Default)]
struct Shared {
    threads: Vec<Value>,
    can_reply: bool,
    can_resolve: bool,
    writable: bool,
    why: Option<String>,
}

/// Whether a thread has a line of this file's diff to sit under.
fn on_a_line(file: &FileDiff, thread: &Value) -> bool {
    let Some((side, number)) = lines::thread_anchor(thread) else { return false };
    file.hunks.iter().any(|hunk| hunk.lines.iter().any(|line| lines::is_on(&side, number, line)))
}
