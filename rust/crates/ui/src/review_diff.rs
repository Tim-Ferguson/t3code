//! The diff panel's "Changes" and "Uncommitted" scopes: reading the checkout through
//! `review.getDiffPreview`, keeping it fresh, and reading one file at a time when the server
//! truncated the preview. Port of the review half of `DiffPanel.tsx` and `useReviewFilePatches`.
//!
//! What triggers a read again, as in the original: the refresh button, the window gaining focus,
//! a command or file change finishing in the thread, a new base ref, and the whitespace switch.
//! A read that finds the same `diffHash` leaves what is drawn alone.
use crate::{
    diff_panel::use_can_files,
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
};
use dioxus::{dioxus_core::Task, prelude::*};
use serde_json::json;
use std::{cell::{Cell, RefCell}, collections::{BTreeMap, BTreeSet}, rc::Rc};
use t3_client::{
    checkpoints::{Load, Shown},
    connection::ConnectionStatus,
    diff::{self, FileDiff, Renderable, Stat},
    review::{self, FileStat, Preview, Source, SourceKind},
};

/// What one scope of a preview shows.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Body {
    Empty,
    Raw { text: String, reason: &'static str },
    Files(Vec<Rc<FileDiff>>),
    /// Truncated with statistics: the files by name, each patch read on demand.
    Lazy(Vec<FileStat>),
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Selected {
    pub kind: SourceKind,
    pub cwd: String,
    pub base_ref: Option<String>,
    pub head_ref: Option<String>,
    pub hash: String,
    pub truncated: bool,
    pub has_stats: bool,
    pub body: Body,
    pub stat: Stat,
}
impl Selected {
    /// Names what is drawn; sections are keyed by it so an unchanged diff is not drawn again.
    pub fn key(&self) -> String {
        format!("{}:{}", self.kind.wire(), self.hash)
    }
}

pub(crate) fn select(preview: &Preview, kind: SourceKind) -> Option<Rc<Selected>> {
    let source = preview.source(kind)?;
    let (body, stat) = if source.lazy() {
        let files = source.sorted_files();
        let stat = files.iter().fold(Stat::default(), |total, file| Stat { additions: total.additions + file.additions, deletions: total.deletions + file.deletions });
        (Body::Lazy(files), stat)
    } else {
        let stat_of_files = source.files.as_ref().map(|files| files.iter().fold(Stat::default(), |total, file| Stat { additions: total.additions + file.additions, deletions: total.deletions + file.deletions }));
        match diff::renderable(&source.diff) {
            None => (Body::Empty, stat_of_files.unwrap_or_default()),
            Some(Renderable::Raw { text, reason }) => (Body::Raw { text, reason }, stat_of_files.unwrap_or_default()),
            Some(Renderable::Files(mut files)) => {
                files.sort_by(|a, b| review::natural_cmp(a.path(), b.path()));
                let stat = stat_of_files.unwrap_or_else(|| diff::total_stat(&files));
                (Body::Files(files.into_iter().map(Rc::new).collect()), stat)
            }
        }
    };
    Some(Rc::new(Selected {
        kind,
        cwd: preview.cwd.clone(),
        base_ref: source.base_ref.clone(),
        head_ref: source.head_ref.clone(),
        hash: source.diff_hash.clone(),
        truncated: source.truncated,
        has_stats: source.files.is_some(),
        body,
        stat,
    }))
}

/// One file of a lazily read source.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum FilePatch {
    Loading,
    Ready { file: Rc<FileDiff>, truncated: bool },
    Failed,
}
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Lazy {
    /// Names the source the patches are of; another source starts over.
    key: String,
    pub patches: BTreeMap<String, FilePatch>,
    /// Files read from the top, in steps, and files the reader opened out of order.
    batch: usize,
    extra: BTreeSet<String>,
}

#[derive(Clone, Copy)]
pub(crate) struct Review {
    pub preview: Signal<Load<Rc<Preview>>>,
    pub refreshing: Signal<bool>,
    pub error: Signal<Option<String>>,
    pub lazy: Signal<Lazy>,
    /// What the scope in view shows of the preview; `None` until it has been read.
    pub selected: Memo<Option<Rc<Selected>>>,
    refresh_count: Signal<u32>,
}
impl Review {
    pub fn refresh(mut self) {
        let next = *self.refresh_count.peek() + 1;
        self.refresh_count.set(next);
    }
    /// Reads the next few files of a lazily read source.
    pub fn load_next(mut self) {
        let mut lazy = self.lazy.write();
        lazy.batch = lazy.batch.max(review::FILE_BATCH) + review::FILE_BATCH;
    }
    /// Reads this file now, wherever it is in the list; also the retry of a failed one.
    pub fn request(mut self, path: &str) {
        let mut lazy = self.lazy.write();
        if matches!(lazy.patches.get(path), Some(FilePatch::Failed)) {
            lazy.patches.remove(path);
        }
        lazy.extra.insert(path.to_owned());
    }
}

/// The key of a preview request: the checkout, the base asked for, and whitespace.
pub(crate) fn preview_key(cwd: &str, base_ref: Option<&str>, ignore_whitespace: bool) -> String {
    json!([cwd, base_ref, ignore_whitespace]).to_string()
}

/// Reads the checkout while `active`, keeps it fresh, and reads the patches of a truncated
/// preview on demand. Everything it starts ends when the panel does.
pub(crate) fn use_review(
    state: Store<UiModel>,
    transport: TransportHandle,
    cwd: String,
    thread_id: String,
    kind: Memo<Option<SourceKind>>,
    base_ref: Signal<Option<String>>,
    ignore_whitespace: Signal<bool>,
) -> Review {
    let preview = use_signal(Load::<Rc<Preview>>::default);
    let active = use_memo(move || kind().is_some());
    // The patch is parsed once per content: a read that finds the same hash keeps the same value.
    let parsed = use_hook(|| Rc::new(RefCell::new(None::<Rc<Selected>>)));
    let selected = use_memo(move || {
        let kind = kind()?;
        let Load::Ready { value, .. } = &*preview.read() else { return None };
        let source = value.source(kind)?;
        if let Some(last) = parsed.borrow().as_ref().filter(|last| last.kind == kind && last.hash == source.diff_hash && last.truncated == source.truncated && last.cwd == value.cwd && last.base_ref == source.base_ref) {
            return Some(last.clone());
        }
        let made = select(value, kind);
        *parsed.borrow_mut() = made.clone();
        made
    });
    let refreshing = use_signal(|| false);
    let error = use_signal(|| None::<String>);
    let lazy = use_signal(Lazy::default);
    let mut refresh_count = use_signal(|| 0u32);
    let can_files = use_can_files(state);
    let task = use_hook(|| Rc::new(RefCell::new(None::<Task>)));
    let seen_refresh = use_hook(|| Rc::new(Cell::new(0u32)));
    // Whether the connection could read the checkout the last time the read below was decided.
    let was_up = use_hook(|| Rc::new(Cell::new(false)));

    {
        let (task, transport, cwd, seen, was_up) = (task.clone(), transport.clone(), cwd.clone(), seen_refresh, was_up);
        use_effect(move || {
            let (up, base, whitespace, attempt) = (can_files(), base_ref.read().clone(), *ignore_whitespace.read(), refresh_count());
            let wanted = active() && up;
            // A connection that returns reads the checkout again: what is shown was read before it was lost.
            let was = was_up.replace(up);
            let returned = up && !was;
            if let Some(previous) = task.borrow_mut().take() {
                previous.cancel();
            }
            let mut preview = preview;
            let (mut refreshing, mut error) = (refreshing, error);
            if !wanted {
                refreshing.set(false);
                return;
            }
            let key = preview_key(&cwd, base.as_deref(), whitespace);
            let have = matches!(preview.peek().shown(&key), Shown::Ready(_));
            if have && attempt == seen.get() && !returned {
                return;
            }
            seen.set(attempt);
            if have {
                refreshing.set(true);
            } else {
                preview.write().start(key.clone());
                refreshing.set(false);
            }
            let (transport, cwd) = (transport.clone(), cwd.clone());
            let owner = runtime::response_owner(&transport, state);
            let running = spawn(async move {
                let ask = |at: String| {
                    let (transport, base) = (transport.clone(), base.clone());
                    async move { runtime::request_value_reported(transport, state, review::DIFF_PREVIEW, review::preview_payload(&at, base.as_deref(), whitespace, None)).await }
                };
                let mut answer = ask(cwd.clone()).await;
                if let Err(message) = &answer {
                    let server_cwd = state.peek().config["cwd"].as_str().map(str::to_owned);
                    if let Some(other) = review::retry_at_server_cwd(message, &cwd, server_cwd.as_deref()) {
                        answer = ask(other).await;
                    }
                }
                if owner.is_none() || owner != runtime::response_owner(&transport, state) {
                    return;
                }
                let outcome = answer.and_then(|value| review::decode(&value));
                match outcome {
                    Ok(new) => {
                        let unchanged = matches!(preview.peek().shown(&key), Shown::Ready(old) if old.same_content(&new));
                        if have && !unchanged {
                            if let Load::Ready { value, .. } = &mut *preview.write() {
                                *value = Rc::new(new);
                            }
                        } else if !have {
                            preview.write().finish(&key, Ok(Rc::new(new)));
                        }
                        error.set(None);
                    }
                    Err(message) if have => error.set(Some(message)),
                    Err(message) => {
                        preview.write().finish(&key, Err(message));
                    }
                }
                refreshing.set(false);
            });
            *task.borrow_mut() = Some(running);
        });
    }
    use_drop({
        let task = task.clone();
        move || {
            if let Some(running) = task.borrow_mut().take() {
                running.cancel();
            }
        }
    });

    // The window gaining focus: the reader has probably been in an editor or a terminal.
    // The listener ends with the panel; the page script waits for its stop message.
    {
        let watching = use_hook(|| Rc::new(RefCell::new(None::<document::Eval>)));
        use_effect({
            let watching = watching.clone();
            move || {
                let wanted = active();
                let mut slot = watching.borrow_mut();
                if !wanted {
                    if let Some(old) = slot.take() {
                        let _ = old.send(true);
                    }
                    return;
                }
                if slot.is_some() {
                    return;
                }
                let mut watch = document::eval(
                    "const on=()=>dioxus.send(true);window.addEventListener('focus',on);await dioxus.recv();window.removeEventListener('focus',on);",
                );
                *slot = Some(watch.clone());
                spawn(async move {
                    while watch.recv::<bool>().await.is_ok() {
                        let next = *refresh_count.peek() + 1;
                        refresh_count.set(next);
                    }
                });
                // Tests cannot focus a window; they say it did.
                #[cfg(test)]
                {
                    let (sender, mut focused) = futures_channel::mpsc::unbounded::<()>();
                    WINDOW_FOCUS.with(|senders| senders.borrow_mut().push(sender));
                    spawn(async move {
                        use futures_util::StreamExt;
                        while focused.next().await.is_some() {
                            let next = *refresh_count.peek() + 1;
                            refresh_count.set(next);
                        }
                    });
                }
            }
        });
        use_drop(move || {
            if let Some(old) = watching.borrow_mut().take() {
                let _ = old.send(true);
            }
        });
    }

    // A command or file change that finishes in the thread may have changed the checkout.
    {
        let mut last = use_signal(|| None::<String>);
        let mutation = use_memo(move || review::workspace_mutation(state.thread().read().projection.as_ref(), &thread_id));
        use_effect(move || {
            let now = mutation();
            if *last.peek() == now {
                return;
            }
            let first = last.peek().is_none();
            last.set(now);
            // The first value is what was there when the panel opened; the read for it is the
            // panel's own.
            if !first && *active.peek() {
                let next = *refresh_count.peek() + 1;
                refresh_count.set(next);
            }
        });
    }

    // Patches of a truncated preview: the first few files, then the ones the reader asks for.
    {
        let mut lazy = lazy;
        use_effect(move || {
            let Some(selected) = selected() else { return };
            let Body::Lazy(files) = &selected.body else { return };
            let key = selected.key();
            let current = lazy.read().clone();
            if current.key != key {
                lazy.set(Lazy { key, batch: review::FILE_BATCH, ..Lazy::default() });
                return;
            }
            let wanted: Vec<FileStat> = files
                .iter()
                .enumerate()
                .filter(|(index, file)| (*index < current.batch || current.extra.contains(&file.path)) && !current.patches.contains_key(&file.path))
                .map(|(_, file)| file.clone())
                .collect();
            if wanted.is_empty() {
                return;
            }
            {
                let mut write = lazy.write();
                for file in &wanted {
                    write.patches.insert(file.path.clone(), FilePatch::Loading);
                }
            }
            let (transport, selected) = (transport.clone(), selected.clone());
            let (key, cwd, base) = (selected.key(), selected.cwd.clone(), selected.base_ref.clone());
            let owner = runtime::response_owner(&transport, state);
            for file in wanted {
                let (transport, key, cwd, base, kind) = (transport.clone(), key.clone(), cwd.clone(), base.clone(), selected.kind);
                let owner = owner.clone();
                spawn(async move {
                    let payload = review::preview_payload(&cwd, base.as_deref(), *ignore_whitespace.peek(), Some((&file, kind)));
                    let answer = runtime::request_value_reported(transport.clone(), state, review::DIFF_PREVIEW, payload).await;
                    if owner.is_none() || owner != runtime::response_owner(&transport, state) {
                        return;
                    }
                    let patch = answer.and_then(|value| review::decode(&value)).ok().and_then(|preview| preview.source(kind).cloned()).and_then(|source| patch_of(&source, &file));
                    let mut write = lazy.write();
                    // Another source took the list over while the server worked.
                    if write.key != key {
                        return;
                    }
                    write.patches.insert(file.path.clone(), patch.unwrap_or(FilePatch::Failed));
                });
            }
        });
    }

    Review { preview, refreshing, error, lazy, selected, refresh_count }
}

/// The one file a per-file read answers with. A patch that does not hold the file is a failure.
fn patch_of(source: &Source, file: &FileStat) -> Option<FilePatch> {
    let Renderable::Files(files) = diff::renderable(&source.diff)? else { return None };
    let found = files.into_iter().find(|candidate| candidate.path() == file.path)?;
    Some(FilePatch::Ready { file: Rc::new(found), truncated: source.truncated })
}

/// Whether the connection is up, for the "offline" marking of data that was read before.
pub(crate) fn use_connected(state: Store<UiModel>) -> Memo<bool> {
    use_memo(move || *state.status().read() == ConnectionStatus::Connected)
}

#[cfg(test)]
thread_local! {
    static WINDOW_FOCUS: RefCell<Vec<futures_channel::mpsc::UnboundedSender<()>>> = const { RefCell::new(Vec::new()) };
}
/// Says the window gained focus, to every diff panel that is watching for it.
#[cfg(test)]
pub(crate) fn window_gained_focus() {
    WINDOW_FOCUS.with(|senders| senders.borrow_mut().retain(|sender| sender.unbounded_send(()).is_ok()));
}
