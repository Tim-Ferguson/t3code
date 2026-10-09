//! The pull request panel's "Files changed" tab: the patch from `POST /api/pull-requests/diff`
//! drawn by the diff panel's own file sections (collapsed files draw nothing, an open one a
//! window of rows, unchanged lines read through `pullRequests.diffFileContents`), and the
//! reader's viewed marks, which live at the host and are written one file at a time.
//!
//! Read only while the tab is mounted. A newer revision (the panel's refresh counter, moved by the
//! server's refresh notice) reads the first page again; an answer for another pull request or
//! connection is dropped. Files past the first page are read when asked for.
use crate::{
    diff_panel::{ExpandHost, ExpandSource, FileSection, Mounts, Pending},
    notice::{self, Level},
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
};
use dioxus::prelude::*;
use serde_json::Value;
use std::{collections::BTreeSet, rc::Rc};
use t3_client::{
    diff::{self, FileDiff},
    pull_request::{self as pr, Reference},
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
pub(crate) fn Files(state: Store<UiModel>, transport: TransportHandle, reference: Reference, revision: u32, readable: bool, writable: bool, why: Option<String>, threads: Vec<Value>, verdicts: Vec<String>, can_resolve: bool, on_changed: Callback<()>) -> Element {
    use_context_provider(Mounts::default);
    let pending = use_signal(|| None::<String>);
    use_context_provider(|| Pending(pending));
    use_context_provider({
        let transport = transport.clone();
        move || ExpandHost { state, transport }
    });
    let mut patch = use_signal(Patch::default);
    let mut viewed = use_signal(BTreeSet::<String>::new);
    let mut viewed_revision = use_signal(|| 0u32);
    let mut toggling = use_signal(|| None::<String>);
    let mut more = use_signal(|| false);
    let mut toggled = use_signal(BTreeSet::<String>::new);
    let mut shown = use_signal(|| FILES_PER_PAGE);

    // The first page, read now and again at each new revision.
    {
        let (transport, reference) = (transport.clone(), reference.clone());
        use_resource(move || {
            let (transport, reference, readable, _revision) = (transport.clone(), reference.clone(), readable, revision);
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
        });
    }
    // Which files the reader has marked viewed; marking one reads this again.
    {
        let (transport, reference) = (transport.clone(), reference.clone());
        use_resource(move || {
            let (transport, reference, readable, _a, _b) = (transport.clone(), reference.clone(), readable, revision, viewed_revision());
            async move {
                if !readable {
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
        });
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
                    crate::pull_request_review::ReviewForm { state, transport: transport.clone(), reference: reference.clone(), verdicts: verdicts.clone(), writable, why: why.clone(), on_done: on_changed }
                    p { class: "pr-files-summary muted", role: "status", "{total} files in this page" if loaded.next_cursor.is_some() { " (more to load)" } }
                    for (index, file) in loaded.files.iter().take(drawn).enumerate() {
                        {
                            let path = file.path().to_owned();
                            let is_viewed = viewed_now.contains(&path);
                            let opened = toggled.read().contains(&path);
                            let collapsed = if is_viewed || index >= OPEN_FILES { !opened } else { opened };
                            let mark_path = path.clone();
                            rsx! {
                                div { key: "{path}", class: "pr-file",
                                    label { class: "pr-viewed", title: why.clone().unwrap_or_default(),
                                        input { r#type: "checkbox", "aria-label": "Viewed {path}", checked: is_viewed, disabled: !writable || busy.is_some(),
                                            onclick: move |event| { event.prevent_default(); mark.call((mark_path.clone(), !is_viewed)); } }
                                        " Viewed"
                                    }
                                    FileSection { file: file.clone(), collapsed, split, on_toggle, stat: None, partial: false, expand: Some(expand.clone()) }
                                    {
                                        let here: Vec<Value> = threads.iter().filter(|thread| thread["path"] == path.as_str()).cloned().collect();
                                        rsx! { if !here.is_empty() { crate::pull_request_review::Threads { state, transport: transport.clone(), reference: reference.clone(), threads: here, can_resolve, writable, why: why.clone(), on_changed } } }
                                    }
                                }
                            }
                        }
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
