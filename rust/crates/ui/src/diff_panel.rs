//! The diff panel: what one turn (or the whole thread) changed, read from the server's
//! checkpoint diffs. Opening, closing and picking a file are model state, so the conversation
//! and sidebar never re-render for them; the per-turn change list is derived from the thread
//! projection behind a memo, so streaming deltas never re-render an open panel either.
//!
//! Large diffs are drawn a window at a time: files start collapsed (a client setting, as in the
//! original), an expanded file draws its first rows and grows on request, and only the lines
//! drawn are syntax highlighted.
use crate::{
    base_ref_picker,
    changed_files::{FileTree, StatLabel},
    review_diff::{self, Body, FilePatch, Selected},
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
    syntax_highlighting::{self, RenderedLine, highlighted_line},
};
use dioxus::prelude::*;
use serde_json::Value;
use std::{cell::{Cell, RefCell}, collections::BTreeMap, rc::Rc};
use t3_client::{
    checkpoints::{self, ChangedFile, Load, Plan, Request, Scope, Shown},
    connection::ConnectionStatus,
    diff::{self, DiffLine, Documents, FileDiff, FileStatus, LineKind, Renderable, Row, Stat},
    review::{self, FileStat, Gap, Reveals, SourceKind},
};
use t3_contracts::{AuthEnvironmentScope, DiffLayout};

/// Rows an expanded file draws before the reader asks for more, and each step after.
const INITIAL_ROWS: usize = 300;
const MORE_ROWS: usize = 500;
/// File sections drawn before "Show more files".
const FILES_PER_PAGE: usize = 200;
/// Diff lines per file that are tokenized for colour; the rest of a huge file stays plain.
const HIGHLIGHT_BUDGET: usize = 3000;

/// The latest request to point the panel somewhere. A request carries the thread it was made for, so it can never steer
/// another thread's panel. Whether the panel is open is the tabbed panel's (`right_panel`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DiffModel {
    pub request: Option<DiffRequest>,
    /// What each thread's panel last showed, so closing and reopening it, or coming back to the
    /// thread, finds the same scope and base ref (the original persists the same pair).
    pub selections: BTreeMap<(String, String), (Scope, Option<String>)>,
}
#[derive(Debug, Clone, PartialEq)]
pub struct DiffRequest {
    pub id: u64,
    pub owner: (String, String),
    pub scope: Scope,
    pub file: Option<String>,
}

/// Opens the panel on a turn, optionally at one of its files.
pub(crate) fn open(state: Store<UiModel>, environment_key: &str, thread_id: &str, scope: Scope, file: Option<String>) {
    {
        let mut diff = state.diff();
        let mut model = diff.write();
        let id = model.request.as_ref().map_or(0, |request| request.id) + 1;
        model.request = Some(DiffRequest { id, owner: (environment_key.to_owned(), thread_id.to_owned()), scope, file });
    }
    let owner = crate::right_panel::Owner { environment: environment_key.to_owned(), thread: Some(thread_id.to_owned()) };
    crate::right_panel::open_kind_in(state, &owner, t3_client::right_panel::Kind::Diff);
}

/// Closes the tab, as its own close button and Escape do.
pub(crate) fn close(state: Store<UiModel>) {
    crate::right_panel::close_kind(state, t3_client::right_panel::Kind::Diff);
}

/// The header button's action, and the `diff.toggle` command's.
pub(crate) fn toggle(state: Store<UiModel>) {
    if !crate::right_panel::shown_now(state, t3_client::right_panel::Kind::Diff) {
        state.diff().write().request = None;
    }
    crate::right_panel::toggle_kind(state, t3_client::right_panel::Kind::Diff);
}

/// The connection's grant to read orchestration data, which both diff RPCs need.
pub(crate) fn use_can_read(state: Store<UiModel>) -> Memo<bool> {
    use_memo(move || {
        let destination = state.destination().read().clone();
        *state.status().read() == ConnectionStatus::Connected
            && destination.is_some_and(|destination| state.environments().read().allows(&destination, AuthEnvironmentScope::OrchestrationRead))
    })
}

/// The connection's grant to read the checkout's files, which the review diffs need.
pub(crate) fn use_can_files(state: Store<UiModel>) -> Memo<bool> {
    use_memo(move || {
        let destination = state.destination().read().clone();
        *state.status().read() == ConnectionStatus::Connected
            && destination.is_some_and(|destination| state.environments().read().allows(&destination, AuthEnvironmentScope::FilesystemRead))
    })
}

/// The workspace header's diff button.
#[component]
pub(crate) fn Toggle(state: Store<UiModel>, environment_key: String, thread_id: String) -> Element {
    let open_now = use_memo(move || crate::right_panel::is_shown(state, t3_client::right_panel::Kind::Diff));
    let allowed = use_can_read(state);
    rsx! {
        button { class: if open_now() { "icon-button diff-toggle selected" } else { "icon-button diff-toggle" }, "aria-label": "Diff", "aria-pressed": "{open_now()}",
            title: if open_now() { "Hide the diff" } else { "Show what this thread changed" }, disabled: !open_now() && !allowed(),
            onclick: move |_| toggle(state),
            "Diff"
        }
    }
}

/// Which files the reader has opened or closed, for one selection. Another turn starts again
/// from the client setting, as the original's per-selection collapse state does.
#[derive(Clone, Default)]
struct Collapse {
    scope: Scope,
    /// `None` until the reader uses "expand all" or "collapse all": the client setting rules.
    default: Option<bool>,
    overrides: BTreeMap<String, bool>,
}
impl Collapse {
    fn collapsed(&self, scope: &Scope, setting: bool, path: &str) -> bool {
        if &self.scope != scope {
            return setting;
        }
        self.overrides.get(path).copied().or(self.default).unwrap_or(setting)
    }
    /// The state to edit: the reader's, or a fresh one if the selection has moved.
    fn for_scope(&mut self, scope: &Scope) -> &mut Self {
        if &self.scope != scope {
            *self = Collapse { scope: scope.clone(), ..Collapse::default() };
        }
        self
    }
}

/// Elements of the file headers that scrolling can target, by path.
#[derive(Clone, Default)]
pub(crate) struct Mounts(pub(crate) Rc<RefCell<BTreeMap<String, Rc<MountedData>>>>);
/// The file to scroll to once its section exists.
#[derive(Clone, Copy)]
pub(crate) struct Pending(pub(crate) Signal<Option<String>>);

fn scroll_into_view(element: Rc<MountedData>) {
    spawn(async move {
        let _ = element.scroll_to(ScrollBehavior::Instant).await;
    });
}

#[derive(Debug, PartialEq, Eq)]
enum Content {
    /// Both sides are identical, or the range is empty.
    Empty,
    Files(Vec<Rc<FileDiff>>),
    Raw { text: String, reason: &'static str },
}
#[derive(Debug, PartialEq, Eq)]
struct Loaded {
    content: Content,
    stat: Stat,
    changed: Vec<ChangedFile>,
}

fn decode(value: Value, thread_id: &str, request: &Request) -> Result<Rc<Loaded>, String> {
    let result: t3_contracts::ThreadTurnDiff = serde_json::from_value(value).map_err(|error| format!("Invalid diff response: {error}"))?;
    if result.thread_id.as_str() != thread_id
        || Some(result.to_turn_count.0) != request.payload["toTurnCount"].as_u64()
        || result.from_turn_count.0 != request.payload["fromTurnCount"].as_u64().unwrap_or(0)
    {
        return Err("The server answered for a different diff.".into());
    }
    Ok(Rc::new(match diff::renderable(&result.diff) {
        None => Loaded { content: Content::Empty, stat: Stat::default(), changed: Vec::new() },
        Some(Renderable::Raw { text, reason }) => Loaded { content: Content::Raw { text, reason }, stat: Stat::default(), changed: Vec::new() },
        Some(Renderable::Files(files)) => Loaded {
            stat: diff::total_stat(&files),
            changed: files
                .iter()
                .map(|file| ChangedFile { path: file.path().to_owned(), kind: format!("{:?}", file.status).to_lowercase(), additions: Some(file.additions.into()), deletions: Some(file.deletions.into()) })
                .collect(),
            content: Content::Files(files.into_iter().map(Rc::new).collect()),
        },
    }))
}

#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct Gate {
    pub hold: Rc<Cell<bool>>,
    pub held: Rc<RefCell<Vec<(Value, futures_channel::oneshot::Sender<()>)>>>,
}

#[component]
pub(crate) fn Panel(state: Store<UiModel>, transport: TransportHandle, environment_key: String, thread_id: String, cwd: String) -> Element {
    #[cfg(test)]
    if let Some(probe) = try_consume_context::<crate::RenderProbe>() {
        probe.diff.set(probe.diff.get() + 1);
    }
    let owner = (environment_key.clone(), thread_id.clone());
    let initial = state.diff().peek().request.clone().filter(|request| request.owner == owner);
    let seen = use_hook({
        let id = initial.as_ref().map_or(0, |request| request.id);
        move || Rc::new(Cell::new(id))
    });
    let remembered = state.diff().peek().selections.get(&owner).cloned();
    let mut scope = use_signal({
        let scope = initial.as_ref().map(|request| request.scope.clone()).or_else(|| remembered.as_ref().map(|(scope, _)| scope.clone())).unwrap_or_default();
        move || scope
    });
    let mut base_ref = use_signal({
        let base = remembered.and_then(|(_, base)| base);
        move || base
    });
    let mut base_open = use_signal(|| false);
    let mut focus = use_signal({
        let file = initial.and_then(|request| request.file);
        move || file
    });
    let mut reveal = use_signal(|| 1u64);
    let mut ignore_whitespace = use_signal(|| state.client_settings().peek().diff_ignore_whitespace);
    let mut wrap = use_signal(|| state.client_settings().peek().word_wrap);
    let mut collapse = use_signal(Collapse::default);
    let mut tree_open = use_signal(|| false);
    let mut shown_files = use_signal(|| FILES_PER_PAGE);
    let mut attempt = use_signal(|| 0u32);
    let mut fetch = use_signal(Load::<Rc<Loaded>>::default);
    let mut handled = use_signal(|| None::<(u64, String)>);
    let mut pending = use_signal(|| None::<String>);
    let mounts = use_context_provider(Mounts::default);
    use_context_provider(|| Pending(pending));
    let split = state.client_settings().read().diff_layout == DiffLayout::Split;
    #[cfg(test)]
    let gate = use_hook(|| try_consume_context::<Gate>());

    // A click in the conversation while the panel is open re-aims it.
    {
        let owner = owner.clone();
        let seen = seen.clone();
        use_effect(move || {
            let Some(request) = state.diff().read().request.clone() else { return };
            if request.owner != owner || request.id == seen.get() {
                return;
            }
            seen.set(request.id);
            scope.set(request.scope);
            focus.set(request.file);
            { let next = *reveal.peek() + 1; reveal.set(next); }
        });
    }

    let turns = use_memo({
        let thread_id = thread_id.clone();
        move || checkpoints::summarize(state.thread().read().projection.as_ref(), &thread_id)
    });
    let can_read = use_can_read(state);
    let can_files = use_can_files(state);
    let connected = review_diff::use_connected(state);
    let kind = use_memo(move || match *scope.read() {
        Scope::Changes => Some(SourceKind::BranchRange),
        Scope::Uncommitted => Some(SourceKind::WorkingTree),
        _ => None,
    });
    let review = review_diff::use_review(state, transport.clone(), cwd.clone(), thread_id.clone(), kind, base_ref, ignore_whitespace);
    use_context_provider(|| ExpandHost { state, transport: transport.clone() });
    let commenting = use_context_provider(|| Commenter(Signal::new(None)));
    {
        let owner = owner.clone();
        use_effect(move || {
            let entry = (scope.read().clone(), base_ref.read().clone());
            if state.diff().peek().selections.get(&owner) != Some(&entry) {
                state.diff().write().selections.insert(owner.clone(), entry);
            }
        });
    }
    let plan = use_memo({
        let thread_id = thread_id.clone();
        move || checkpoints::plan(&turns.read(), &scope.read(), &thread_id, *ignore_whitespace.read())
    });
    // "Latest" becomes the turn it names, so a diff in view does not change when another turn ends.
    use_effect(move || {
        if *scope.read() != Scope::Latest {
            return;
        }
        if let Some(latest) = turns.read().latest() {
            scope.set(Scope::Turn(latest.run_id.clone()));
        }
    });

    // A turn that was rewound leaves the list; the selection falls back to the newest turn
    // instead of naming something the server can no longer diff.
    use_effect(move || {
        let Scope::Turn(run_id) = scope.read().clone() else { return };
        let gone = {
            let turns = turns.read();
            turns.loaded && !turns.turns.iter().any(|turn| turn.run_id == run_id)
        };
        if gone {
            scope.set(Scope::Latest);
        }
    });

    let task = use_hook(|| Rc::new(RefCell::new(None::<dioxus::dioxus_core::Task>)));
    {
        let (task, transport, thread_id) = (task.clone(), transport.clone(), thread_id.clone());
        use_effect(move || {
            let plan = plan();
            let attempt = attempt();
            let allowed = can_read();
            // Whatever was asking for the previous selection stops, and tells the server.
            if let Some(previous) = task.borrow_mut().take() {
                previous.cancel();
            }
            let Plan::Fetch(request) = plan else {
                fetch.set(Load::Idle);
                return;
            };
            if !allowed {
                return;
            }
            let key = format!("{}#{attempt}", request.key);
            if matches!(fetch.peek().shown(&key), Shown::Ready(_)) {
                return;
            }
            fetch.write().start(key.clone());
            let owner = runtime::response_owner(&transport, state);
            let (transport, thread_id) = (transport.clone(), thread_id.clone());
            #[cfg(test)]
            let gate = gate.clone();
            let running = spawn(async move {
                #[cfg(test)]
                if let Some(gate) = gate.filter(|gate| gate.hold.get()) {
                    let (release, held) = futures_channel::oneshot::channel();
                    gate.held.borrow_mut().push((request.payload.clone(), release));
                    let _ = held.await;
                }
                let outcome = runtime::request_value(transport.clone(), state, request.method, request.payload.clone())
                    .await
                    .and_then(|value| decode(value, &thread_id, &request));
                // The connection or thread may have changed while the server worked.
                if owner.is_none() || owner != runtime::response_owner(&transport, state) {
                    return;
                }
                if !matches!(&*fetch.peek(), Load::Loading { key: current } if current == &key) {
                    return;
                }
                fetch.write().finish(&key, outcome);
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

    // What the effects and the render below agree the diff on screen is for. These are memos,
    // not locals: an effect keeps the closure it was created with, so it must read signals.
    let current_key = use_memo(move || match &*plan.read() {
        Plan::Fetch(request) => format!("{}#{}", request.key, attempt()),
        _ => String::new(),
    });
    let loaded = use_memo(move || match fetch.read().shown(&current_key.read()) {
        Shown::Ready(loaded) => Some(loaded.clone()),
        _ => None,
    });

    // Bring a requested file into view: open it, make sure its section is drawn, scroll to it.
    use_effect(move || {
        let (Some(path), Some(loaded)) = (focus.read().clone(), loaded()) else { return };
        let applied = Some((*reveal.read(), current_key.peek().clone()));
        if *handled.peek() == applied {
            return;
        }
        handled.set(applied);
        let Content::Files(files) = &loaded.content else { return };
        let Some(index) = files.iter().position(|file| file.path() == path) else { return };
        collapse.write().for_scope(&scope.peek()).overrides.insert(path.clone(), false);
        if *shown_files.peek() <= index {
            shown_files.set(index + 1);
        }
        pending.set(Some(path));
    });
    {
        let mounts = mounts.clone();
        use_effect(move || {
            let Some(path) = pending.read().clone() else { return };
            let element = mounts.0.borrow().get(&path).cloned();
            if let Some(element) = element {
                pending.set(None);
                scroll_into_view(element);
            }
        });
    }

    let loaded_now = loaded();
    let reviewing = kind().is_some();
    let selected: Option<Rc<Selected>> = review.selected.read().clone();
    let review_key = review_diff::preview_key(&cwd, base_ref.read().as_deref(), *ignore_whitespace.read());
    let key_now = if reviewing { selected.as_ref().map(|selected| selected.key()).unwrap_or_default() } else { current_key() };
    let toggle_file = use_callback(move |path: String| {
        let setting = state.client_settings().peek().diff_files_collapsed;
        let collapsed = collapse.peek().collapsed(&scope.peek(), setting, &path);
        collapse.write().for_scope(&scope.peek()).overrides.insert(path, !collapsed);
    });
    let select_file = use_callback(move |path: String| {
        focus.set(Some(path));
        { let next = *reveal.peek() + 1; reveal.set(next); }
    });
    let request_file = use_callback(move |path: String| review.request(&path));

    let turns_now = turns.read().clone();
    let scope_now = scope.read().clone();
    let label = turns_now.label(&scope_now);
    let value = match &scope_now {
        Scope::Changes => "branch".to_owned(),
        Scope::Uncommitted => "unstaged".to_owned(),
        Scope::Thread => "thread".to_owned(),
        Scope::Turn(run_id) if turns_now.latest().is_some_and(|latest| &latest.run_id != run_id) => format!("turn:{run_id}"),
        _ => "latest".to_owned(),
    };
    let older: Vec<(String, u64)> = turns_now.turns.iter().skip(1).map(|turn| (turn.run_id.clone(), turn.ordinal)).collect();
    let lazy_now = review.lazy.read().clone();
    let lazy = matches!(selected.as_deref(), Some(Selected { body: Body::Lazy(_), .. })) && reviewing;
    let entries: Vec<Entry> = if reviewing {
        match selected.as_deref().map(|selected| &selected.body) {
            Some(Body::Files(files)) => files.iter().map(|file| Entry::Ready { file: file.clone(), stat: None, partial: false }).collect(),
            Some(Body::Lazy(stats)) => stats
                .iter()
                .map(|stat| match lazy_now.patches.get(&stat.path) {
                    Some(FilePatch::Ready { file, truncated }) => Entry::Ready { file: file.clone(), stat: Some((stat.additions, stat.deletions)), partial: *truncated },
                    other => Entry::Pending { stat: stat.clone(), loading: matches!(other, Some(FilePatch::Loading)), failed: matches!(other, Some(FilePatch::Failed)), requested: other.is_some() },
                })
                .collect(),
            _ => Vec::new(),
        }
    } else {
        match loaded_now.as_deref() {
            Some(Loaded { content: Content::Files(files), .. }) => files.iter().map(|file| Entry::Ready { file: file.clone(), stat: None, partial: false }).collect(),
            _ => Vec::new(),
        }
    };
    let setting_collapsed = state.client_settings().read().diff_files_collapsed;
    let collapsed_of = |path: &str| collapse.read().collapsed(&scope_now, setting_collapsed, path);
    let all_collapsed = !entries.is_empty() && entries.iter().all(|entry| collapsed_of(entry.path()));
    let stat = if reviewing { selected.as_ref().map(|selected| selected.stat) } else { loaded_now.as_ref().map(|loaded| loaded.stat) }.filter(|stat| stat.additions + stat.deletions > 0);
    let shown_count = (*shown_files.read()).min(entries.len());
    let ignoring = *ignore_whitespace.read();
    let wrapping = *wrap.read();
    let tree_visible = *tree_open.read() && !entries.is_empty();
    let selected_path = focus.read().clone();
    let set_layout = move |layout: DiffLayout| {
        if let Some(writer) = try_consume_context::<crate::client_settings::Writer>() {
            writer.patch(state, serde_json::json!({"diffLayout": if layout == DiffLayout::Split { "split" } else { "stacked" }}));
        } else {
            state.client_settings().write().diff_layout = layout;
        }
    };
    let retry_label = "Retry loading diff";
    let expand_source = selected.as_ref().filter(|_| reviewing).map(|selected| Rc::new(ExpandSource { cwd: selected.cwd.clone(), kind: selected.kind, base_ref: selected.base_ref.clone(), head_ref: selected.head_ref.clone(), pull_request: None }));
    let compared = selected.as_ref().filter(|selected| reviewing && selected.kind == SourceKind::BranchRange).and_then(|selected| selected.base_ref.clone().map(|base| (selected.head_ref.clone(), base)));
    let refreshing = *review.refreshing.read();
    let offline = reviewing && !connected();
    let totals = if selected.as_ref().is_some_and(|selected| selected.has_stats) { " Totals include all changes." } else { "" };
    let pending_unrequested = entries.iter().filter(|entry| matches!(entry, Entry::Pending { requested: false, .. })).count();

    // The file sections both kinds of diff draw: whole files for a checkpoint or a complete
    // preview, and a list that fills in file by file for a truncated one.
    let list = rsx! {
        div { class: "diff-results",
            div { class: "diff-files",
                for entry in entries.iter().take(shown_count) {
                    match entry {
                        Entry::Ready { file, stat, partial } => rsx! {
                            FileSection { key: "{key_now}:{file.path()}", file: file.clone(), collapsed: collapsed_of(file.path()), split, on_toggle: toggle_file, stat: *stat, partial: *partial, expand: expand_source.clone() }
                        },
                        Entry::Pending { stat, loading, failed, .. } => rsx! {
                            PendingFile { key: "{key_now}:{stat.path}:pending", stat: stat.clone(), loading: *loading, failed: *failed, on_request: request_file }
                        },
                    }
                }
                if shown_count < entries.len() {
                    button { class: "diff-more", "aria-label": "Show more files", onclick: move |_| shown_files.set(shown_count + FILES_PER_PAGE),
                        "Show {(entries.len() - shown_count).min(FILES_PER_PAGE)} more files ({entries.len() - shown_count} not shown)" }
                }
                if lazy && pending_unrequested > 0 {
                    button { class: "diff-more", "aria-label": "Load more file diffs", onclick: move |_| review.load_next(),
                        "Load the next {pending_unrequested.min(review::FILE_BATCH)} file diffs ({pending_unrequested} not loaded)" }
                }
            }
            if tree_visible {
                aside { class: "diff-tree",
                    FileTree { files: entries.iter().map(Entry::changed).collect::<Vec<_>>(), expanded: true, selected: selected_path, action: "Go to", label: "Changed files tree", on_select: select_file, on_context: crate::link_menus::file_context() }
                }
            }
        }
    };
    let raw = |text: &str, reason: &str| rsx! {
        div { class: "diff-raw",
            p { class: "muted", "{reason}" }
            pre { "{text}" }
        }
    };

    rsx! {
        aside { class: "diff-panel", "aria-label": "Diff panel", tabindex: "-1", "data-wrap": "{wrapping}", "data-layout": if split { "split" } else { "stacked" },
            onkeydown: move |event| { if event.key() == Key::Escape { if *base_open.peek() { base_open.set(false); } else { close(state); } } },
            if let Some(target) = commenting.0.read().clone() {
                CommentDialog { state, environment_key: environment_key.clone(), thread_id: thread_id.clone(), section: section_of(&scope.read()), target, onclose: move |_| { let mut signal = commenting.0; signal.set(None); } }
            }
            header { class: "diff-header",
                div { class: "diff-header-main",
                    select { class: "diff-scope", "aria-label": "Diff scope", title: "{label}", value: "{value}",
                        onchange: move |event| {
                            let choice = event.value();
                            focus.set(None);
                            base_open.set(false);
                            scope.set(match choice.as_str() {
                                "branch" => Scope::Changes,
                                "unstaged" => Scope::Uncommitted,
                                "thread" => Scope::Thread,
                                "latest" => turns.peek().latest().map_or(Scope::Latest, |latest| Scope::Turn(latest.run_id.clone())),
                                other => Scope::Turn(other.strip_prefix("turn:").unwrap_or(other).to_owned()),
                            });
                        },
                        option { value: "branch", selected: value == "branch", "Changes" }
                        option { value: "unstaged", selected: value == "unstaged", "Uncommitted" }
                        option { value: "latest", selected: value == "latest", "Latest turn" }
                        option { value: "thread", selected: value == "thread", "Whole thread" }
                        for (run_id, ordinal) in older {
                            option { key: "{run_id}", value: "turn:{run_id}", selected: value == format!("turn:{run_id}"), "Turn {ordinal}" }
                        }
                    }
                    if let Some((head, base)) = compared.clone() {
                        span { class: "diff-compare", "aria-label": "Comparing {head.clone().unwrap_or_else(|| \"HEAD\".to_owned())} against {base}", title: "{head.clone().unwrap_or_else(|| \"HEAD\".to_owned())} \u{2192} {base}",
                            span { class: "diff-compare-head", "{head.clone().unwrap_or_else(|| \"HEAD\".to_owned())}" }
                            " \u{2192} "
                            button { class: "diff-compare-base", "aria-label": "Change comparison target. Currently {base}", "aria-haspopup": "dialog", "aria-expanded": "{base_open()}",
                                disabled: !can_files() || offline, onclick: move |_| base_open.set(!base_open()), "{base} \u{25be}" }
                        }
                        if base_open() && can_files() {
                            base_ref_picker::Picker { state, transport: transport.clone(), cwd: cwd.clone(), head: head.clone(), selected: base_ref.read().clone(),
                                on_pick: move |chosen: Option<String>| { base_open.set(false); base_ref.set(chosen); },
                                on_close: move |_| base_open.set(false) }
                        }
                    }
                    if let Some(stat) = stat { StatLabel { additions: stat.additions, deletions: stat.deletions } }
                    if reviewing {
                        button { class: "icon-button diff-refresh", "aria-label": if refreshing { "Refreshing diff" } else { "Refresh diff" }, title: if refreshing { "Refreshing diff\u{2026}" } else { "Refresh diff" },
                            disabled: refreshing || !can_files(), onclick: move |_| review.refresh(), "\u{21bb}" }
                    }
                    button { class: "icon-button diff-close", "aria-label": "Close diff", title: "Close the diff", onclick: move |_| close(state), "✕" }
                }
                div { class: "diff-tools",
                    if !entries.is_empty() {
                        button { "aria-label": if all_collapsed { "Expand all files" } else { "Collapse all files" }, title: if all_collapsed { "Expand all files" } else { "Collapse all files" },
                            onclick: move |_| {
                                let mut collapse = collapse.write();
                                let edit = collapse.for_scope(&scope.peek());
                                edit.overrides.clear();
                                edit.default = Some(!all_collapsed);
                            },
                            if all_collapsed { "Expand all" } else { "Collapse all" } }
                    }
                    div { class: "diff-segmented", role: "group", "aria-label": "Diff layout",
                        button { "aria-label": "Stacked diff view", "aria-pressed": "{!split}", title: "Stacked diff view", onclick: move |_| set_layout(DiffLayout::Stacked), "Stacked" }
                        button { "aria-label": "Split diff view", "aria-pressed": "{split}", title: "Split diff view", onclick: move |_| set_layout(DiffLayout::Split), "Split" }
                    }
                    button { "aria-label": if wrapping { "Disable diff line wrapping" } else { "Enable diff line wrapping" }, "aria-pressed": "{wrapping}", title: if wrapping { "Disable line wrapping" } else { "Enable line wrapping" },
                        onclick: move |_| wrap.set(!wrapping), "Wrap" }
                    button { "aria-label": if ignoring { "Show whitespace changes" } else { "Hide whitespace changes" }, "aria-pressed": "{ignoring}", title: if ignoring { "Show whitespace changes" } else { "Hide whitespace changes" },
                        onclick: move |_| ignore_whitespace.set(!ignoring), "¶" }
                    if !entries.is_empty() {
                        button { "aria-label": if tree_visible { "Hide file tree" } else { "Show file tree" }, "aria-pressed": "{tree_visible}", title: if tree_visible { "Hide file tree" } else { "Show file tree" },
                            onclick: move |_| tree_open.set(!tree_visible), "Files" }
                    }
                }
            }
            div { class: "diff-content",
                if reviewing {
                    if !*can_files.read() {
                        p { class: "diff-message", role: "status", {if *connected.read() { "This connection cannot read local diffs." } else { "Reconnect to load this diff." }} }
                    } else {
                        if offline { p { class: "diff-banner", role: "status", "Offline. This is the last diff read from this checkout." } }
                        if let Some(message) = review.error.read().clone() { p { class: "diff-banner failure", role: "alert", "Could not refresh the diff: {message}" } }
                        match review.preview.read().shown(&review_key) {
                            Shown::Loading => rsx! { LoadingSkeleton { label: kind().map_or("Loading changes...", |kind| kind.loading()) } },
                            Shown::Failed(message) => rsx! {
                                div { class: "diff-failed", role: "alert",
                                    p { class: "diff-error", "{message}" }
                                    button { "aria-label": "{retry_label}", onclick: move |_| review.refresh(), "Retry" }
                                }
                            },
                            Shown::Ready(_) => match selected.as_deref() {
                                None => rsx! { p { class: "diff-message", role: "status", "No patch available for this selection." } },
                                Some(selected) => rsx! {
                                    if selected.truncated && !lazy {
                                        p { class: "diff-banner", role: "status", "This preview exceeds the size limit. Changes shown are incomplete.{totals}" }
                                    }
                                    match &selected.body {
                                        Body::Empty => rsx! { p { class: "diff-message", role: "status", "No net changes in this selection." } },
                                        Body::Raw { text, reason } => raw(text, reason),
                                        Body::Files(_) | Body::Lazy(_) => list.clone(),
                                    }
                                },
                            },
                        }
                    }
                } else if let Some(message) = plan.read().message() {
                    p { class: "diff-message", role: "status", "{message}" }
                } else if !*can_read.read() {
                    p { class: "diff-message", role: "status", {if *state.status().read() == ConnectionStatus::Connected { "This connection cannot read thread diffs." } else { "Reconnect to load this diff." }} }
                } else {
                    match fetch.read().shown(&key_now) {
                        Shown::Loading => rsx! { LoadingSkeleton { label: "Loading checkpoint diff..." } },
                        Shown::Failed(message) => rsx! {
                            div { class: "diff-failed", role: "alert",
                                p { class: "diff-error", "{message}" }
                                button { "aria-label": "{retry_label}", onclick: move |_| attempt.set(attempt() + 1), "Retry" }
                            }
                        },
                        Shown::Ready(_) => match loaded_now.as_deref().map(|loaded| &loaded.content) {
                            Some(Content::Files(_)) => list.clone(),
                            Some(Content::Raw { text, reason }) => raw(text, reason),
                            _ => rsx! { p { class: "diff-message", role: "status", "No net changes in this selection." } },
                        },
                    }
                }
            }
        }
    }
}

/// A file of the list: read, or waiting for its patch (a truncated preview reads them as asked).
enum Entry {
    Ready { file: Rc<FileDiff>, stat: Option<(u64, u64)>, partial: bool },
    Pending { stat: FileStat, loading: bool, failed: bool, requested: bool },
}
impl Entry {
    fn path(&self) -> &str {
        match self {
            Entry::Ready { file, .. } => file.path(),
            Entry::Pending { stat, .. } => &stat.path,
        }
    }
    fn changed(&self) -> ChangedFile {
        match self {
            Entry::Ready { file, .. } => ChangedFile { path: file.path().to_owned(), kind: format!("{:?}", file.status).to_lowercase(), additions: Some(file.additions.into()), deletions: Some(file.deletions.into()) },
            Entry::Pending { stat, .. } => ChangedFile { path: stat.path.clone(), kind: if stat.previous_path.is_some() { "renamed" } else { "modified" }.to_owned(), additions: Some(stat.additions), deletions: Some(stat.deletions) },
        }
    }
}

/// What reading a file's unchanged lines needs to know about its diff.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ExpandSource {
    pub(crate) cwd: String,
    pub(crate) kind: SourceKind,
    pub(crate) base_ref: Option<String>,
    pub(crate) head_ref: Option<String>,
    /// A pull request's file is read through the host, not from a checkout.
    pub(crate) pull_request: Option<t3_client::pull_request::Reference>,
}
#[derive(Clone)]
pub(crate) struct ExpandHost {
    pub(crate) state: Store<UiModel>,
    pub(crate) transport: TransportHandle,
}

/// A file of a truncated preview whose patch has not been read: its header, and a way to read it.
#[component]
fn PendingFile(stat: FileStat, loading: bool, failed: bool, on_request: Callback<String>) -> Element {
    let path = stat.path.clone();
    let (directory, name) = path.rsplit_once('/').map_or(("", path.as_str()), |(directory, name)| (directory, name));
    rsx! {
        section { class: "diff-file diff-pending", "data-file-path": "{path}",
            div { class: "diff-file-header",
                span { class: "diff-file-path",
                    if !directory.is_empty() { span { class: "diff-file-dir", "{directory}/" } }
                    span { class: "diff-file-name", "{name}" }
                    if let Some(previous) = &stat.previous_path { span { class: "diff-file-previous", " \u{2190} {previous}" } }
                }
                StatLabel { additions: stat.additions, deletions: stat.deletions }
                if failed { button { "aria-label": "Retry loading diff for {path}", title: "Retry loading diff", onclick: { let path = path.clone(); move |_| on_request.call(path.clone()) }, "Retry" } }
                else if loading { span { class: "muted", role: "status", "Loading\u{2026}" } }
                else { button { "aria-label": "Load diff for {path}", onclick: { let path = path.clone(); move |_| on_request.call(path.clone()) }, "Load diff" } }
            }
        }
    }
}

/// Static bars where the diff will be. Nothing animates: a loading panel must not cost frames.
#[component]
fn LoadingSkeleton(label: &'static str) -> Element {
    rsx! {
        div { class: "diff-skeleton", role: "status", "aria-live": "polite", "aria-label": "{label}",
            for width in [52, 34, 61, 44] {
                div { class: "diff-skeleton-file", div { class: "diff-skeleton-bar", style: "width: {width}%" } div { class: "diff-skeleton-bar short" } div { class: "diff-skeleton-bar", style: "width: {width + 20}%" } }
            }
            span { class: "sr-only", "{label}" }
        }
    }
}

fn status_mark(status: FileStatus) -> (&'static str, &'static str) {
    match status {
        FileStatus::Added => ("A", "Added"),
        FileStatus::Deleted => ("D", "Deleted"),
        FileStatus::Renamed => ("R", "Renamed"),
        FileStatus::Copied => ("C", "Copied"),
        FileStatus::Modified => ("M", "Modified"),
    }
}

#[component]
pub(crate) fn FileSection(file: Rc<FileDiff>, collapsed: bool, split: bool, on_toggle: Callback<String>, stat: Option<(u64, u64)>, partial: bool, expand: Option<Rc<ExpandSource>>) -> Element {
    let path = file.path().to_owned();
    let mounts = use_context::<Mounts>();
    let pending = use_context::<Pending>();
    use_drop({
        let (mounts, path) = (mounts.clone(), path.clone());
        move || {
            mounts.0.borrow_mut().remove(&path);
        }
    });
    let (letter, status_name) = status_mark(file.status);
    let (directory, name) = path.rsplit_once('/').map_or(("", path.as_str()), |(directory, name)| (directory, name));
    let label = if collapsed { format!("Expand {path}") } else { format!("Collapse {path}") };
    rsx! {
        section { class: "diff-file", "data-file-path": "{path}", "data-collapsed": "{collapsed}",
            onmounted: {
                let (mounts, path) = (mounts.clone(), path.clone());
                move |event: MountedEvent| {
                    let element = event.data();
                    mounts.0.borrow_mut().insert(path.clone(), element.clone());
                    let mut pending = pending.0;
                    if pending.peek().as_deref() == Some(path.as_str()) {
                        pending.set(None);
                        scroll_into_view(element);
                    }
                }
            },
            button { class: "diff-file-header", "aria-label": "{label}", "aria-expanded": "{!collapsed}", title: "{path}",
                onclick: { let path = path.clone(); move |_| on_toggle.call(path.clone()) },
                oncontextmenu: { let (path, on_context) = (path.clone(), crate::link_menus::file_context()); move |event| { if let Some(on_context) = on_context { event.prevent_default(); event.stop_propagation(); let at = event.client_coordinates(); on_context.call((path.clone(), (at.x, at.y))); } } },
                span { class: "diff-file-chevron", "aria-hidden": "true", if collapsed { "▸" } else { "▾" } }
                span { class: "diff-file-status", "data-status": "{letter}", title: "{status_name}", "aria-hidden": "true", "{letter}" }
                span { class: "diff-file-path",
                    if !directory.is_empty() { span { class: "diff-file-dir", "{directory}/" } }
                    span { class: "diff-file-name", "{name}" }
                    if let Some(previous) = file.previous_path() { span { class: "diff-file-previous", " ← {previous}" } }
                }
                if partial { span { class: "diff-partial", role: "img", "aria-label": "Partial diff preview", title: "This file is too large to show in full. Counts include all changes.", "partial" } }
                { let (additions, deletions) = stat.unwrap_or((u64::from(file.additions), u64::from(file.deletions))); rsx! { StatLabel { additions, deletions } } }
            }
            if file.status != FileStatus::Deleted { OpenFile { path: path.clone() } }
            if !collapsed { FileBody { file: file.clone(), split, expand } }
        }
    }
}

/// Opens a changed file in the editor, from the thread's own workspace.
#[component]
fn OpenFile(path: String) -> Element {
    let Some(host) = try_consume_context::<ExpandHost>() else { return rsx! {} };
    let busy = use_signal(|| false);
    let Some(last) = try_consume_context::<crate::open_in::Last>() else { return rsx! {} };
    let state = host.state;
    // A server that offers no editor has nothing to open the file in: no control, not a disabled one.
    if crate::open_in::available(state).is_empty() {
        return rsx! {};
    }
    rsx! { button { class: "diff-file-open", r#type: "button", "aria-label": "Open {path} in editor", title: "Open in editor", disabled: *busy.read(),
        onclick: move |_| {
            let (Some(root), Some(editor)) = (crate::open_in::thread_cwd(state), crate::open_in::favorite(state, last.0.peek().as_deref())) else { return };
            crate::open_in::open(&host.transport, state, busy, last.0, t3_client::file_links::editor_path(&t3_client::file_links::FileTarget { path: path.clone(), line: None, column: None }, &root), editor, false);
        }, "Open" } }
}

/// An expanded file. Files with no hunks (binary, renamed, mode-only) say so instead.
#[component]
fn FileBody(file: Rc<FileDiff>, split: bool, expand: Option<Rc<ExpandSource>>) -> Element {
    if !file.hunks.is_empty() {
        return rsx! { Rows { file, split, expand } };
    }
    let note = if file.binary {
        "Binary file changed. No preview.".to_owned()
    } else if let Some((old, new)) = &file.mode_change {
        format!("File mode changed from {old} to {new}.")
    } else {
        match file.status {
            FileStatus::Added => "Empty file added.",
            FileStatus::Deleted => "File deleted.",
            FileStatus::Renamed => "Renamed without changes.",
            FileStatus::Copied => "Copied without changes.",
            FileStatus::Modified => "No content changes.",
        }
        .to_owned()
    };
    rsx! { p { class: "diff-note", "{note}" } }
}

/// The first rows of a file, highlighted, with the reader deciding how much more to draw. A file
/// of a review diff also offers its unchanged lines: the first click reads the file's new side
/// from the server, and each click after reveals more, merging into the hunks as they meet.
#[component]
fn Rows(file: Rc<FileDiff>, split: bool, expand: Option<Rc<ExpandSource>>) -> Element {
    let mut limit = use_signal(|| INITIAL_ROWS);
    let mut reveals = use_signal(Reveals::new);
    let mut contents = use_signal(Load::<Rc<String>>::default);
    let host = try_consume_context::<ExpandHost>();
    let text = match &*contents.read() {
        Load::Ready { value, .. } => Some(value.clone()),
        _ => None,
    };
    let lines: Vec<&str> = text.as_deref().map(|text| review::text_lines(text)).unwrap_or_default();
    let expanded = if expand.is_some() { review::expand(&file, text.as_ref().map(|_| lines.as_slice()), &reveals.read()) } else { review::Expanded { file: None, gaps: Vec::new() } };
    let shown_file: &FileDiff = expanded.file.as_ref().unwrap_or(&file);
    let window = diff::window(shown_file, split, limit());
    // Colour what is drawn, and no more than the budget, so tokenizing follows the window.
    let drawn: usize = window.hunks.iter().map(|chunk| chunk.lines).sum();
    let documents = Documents::build(shown_file, window.hunks.len(), drawn.min(HIGHLIGHT_BUDGET));
    let language = diff::language_for_path(shown_file.path());
    let old_lines = syntax_highlighting::use_lines(&documents.old, &language);
    let new_lines = syntax_highlighting::use_lines(&documents.new, &language);
    let colour = |hunk: usize, line: &DiffLine| -> Option<Rc<RenderedLine>> {
        if line.kind == LineKind::Deletion {
            let index = documents.old_index(hunk, line)?;
            old_lines.read().as_ref()?.get(index).cloned()
        } else {
            let index = documents.new_index(hunk, line)?;
            new_lines.read().as_ref()?.get(index).cloned()
        }
    };
    let remaining = window.total - window.shown;
    let more = remaining.min(MORE_ROWS);
    let path = file.path().to_owned();
    let notes = line_notes();
    let on_gap = {
        let (file, expand) = (file.clone(), expand.clone());
        use_callback(move |(gap, from_top, all): (Gap, bool, bool)| {
            review::reveal(&mut reveals.write(), &gap, from_top, all);
            let (Some(host), Some(expand)) = (host.clone(), expand.clone()) else { return };
            if matches!(&*contents.peek(), Load::Loading { .. } | Load::Ready { .. }) {
                return;
            }
            contents.write().start("contents".into());
            let (method, payload) = match &expand.pull_request {
                Some(reference) => (t3_client::pull_request::DIFF_FILE_CONTENTS, t3_client::pull_request::contents_payload(reference, &file)),
                None => (review::DIFF_FILE_CONTENTS, review::contents_payload(&expand.cwd, expand.kind, expand.base_ref.as_deref(), expand.head_ref.as_deref(), &file)),
            };
            spawn(async move {
                let owner = runtime::response_owner(&host.transport, host.state);
                let answer = runtime::request_value_reported(host.transport.clone(), host.state, method, payload).await;
                if owner.is_none() || owner != runtime::response_owner(&host.transport, host.state) {
                    return;
                }
                let outcome = answer.and_then(|value| value["newContents"].as_str().map(|text| Rc::new(text.to_owned())).ok_or_else(|| "The server answered without file contents.".to_owned()));
                contents.write().finish("contents", outcome);
            });
        })
    };
    let reading = matches!(&*contents.read(), Load::Loading { .. });
    let read_error = match &*contents.read() {
        Load::Failed { message, .. } => Some(message.clone()),
        _ => None,
    };
    let trailing = expanded.gaps.iter().find(|gap| gap.before >= shown_file.hunks.len()).copied();
    rsx! {
        div { class: "diff-body", "data-layout": if split { "split" } else { "stacked" },
            div { class: "diff-table",
                for chunk in window.hunks.iter() {
                    Fragment { key: "h{chunk.index}",
                    if let Some(gap) = expanded.gaps.iter().find(|gap| gap.before == chunk.index && !gap.trailing) { GapRow { gap: *gap, path: path.clone(), on_expand: on_gap } }
                    div { class: "diff-hunk",
                        div { class: "diff-hunk-header", "{chunk.hunk.header()}" }
                        for (position, row) in chunk.rows.iter().enumerate() {
                            match row {
                                Row::Line(line) => rsx! {
                                    StackedRow { key: "{position}", line: (*line).clone(), colour: colour(chunk.index, line), path: path.clone() }
                                    if let Some(notes) = &notes { { (notes.under)(&path, line, None) } }
                                },
                                Row::Pair(pair) => rsx! {
                                    div { key: "{position}", class: "diff-split-row",
                                        { cell(pair.old, Side::Old, pair.old.and_then(|line| colour(chunk.index, line)), &path) }
                                        { cell(pair.new, Side::New, pair.new.and_then(|line| colour(chunk.index, line)), &path) }
                                    }
                                    if let Some(notes) = &notes {
                                        for (line, right) in [(pair.old, false), (pair.new, true)] {
                                            if let Some(line) = line { { (notes.under)(&path, line, Some(right)) } }
                                        }
                                    }
                                },
                            }
                        }
                    }
                    }
                }
                if remaining == 0 { if let Some(gap) = trailing { GapRow { gap, path: path.clone(), on_expand: on_gap } } }
            }
            if reading { p { class: "diff-note", role: "status", "Loading unchanged lines\u{2026}" } }
            if let Some(message) = read_error { p { class: "diff-error", role: "alert", "{message}" } }
            if remaining > 0 {
                div { class: "diff-more-rows",
                    button { "aria-label": "Show more lines in {path}", onclick: move |_| limit.set(limit() + MORE_ROWS), "Show {more} more lines ({remaining} not shown)" }
                    button { "aria-label": "Show all lines in {path}", onclick: move |_| limit.set(usize::MAX / 4), "Show all" }
                }
            }
        }
    }
}

/// Between two hunks (or before the first, or after the last): the lines nobody has seen.
#[component]
fn GapRow(gap: Gap, path: String, on_expand: Callback<(Gap, bool, bool)>) -> Element {
    let hidden = gap.hidden;
    let step = review::EXPAND_STEP;
    let word = if hidden == 1 { "line" } else { "lines" };
    rsx! {
        div { class: "diff-gap", "data-path": "{path}",
            span { class: "diff-gap-count", "{hidden} unchanged {word}" }
            if hidden <= step {
                button { "aria-label": "Expand {hidden} unchanged {word} in {path}", onclick: move |_| on_expand.call((gap, false, true)), "Expand" }
            } else {
                if gap.index != 0 { button { "aria-label": "Expand {step} unchanged lines down in {path}", onclick: move |_| on_expand.call((gap, true, false)), "\u{2193} {step}" } }
                if !gap.trailing { button { "aria-label": "Expand {step} unchanged lines up in {path}", onclick: move |_| on_expand.call((gap, false, false)), "\u{2191} {step}" } }
                button { "aria-label": "Expand all {hidden} unchanged lines in {path}", onclick: move |_| on_expand.call((gap, false, true)), "All" }
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Side {
    Old,
    New,
}

fn code(line: &DiffLine, colour: Option<Rc<RenderedLine>>) -> Element {
    rsx! {
        span { class: "diff-code",
            if let Some(colour) = colour { highlighted_line { line: colour } } else { "{line.text}" }
            if line.no_newline { span { class: "diff-nonl", "No newline at end of file" } }
        }
    }
}

/// One half of a split row. A missing side is an empty cell, so the columns stay aligned.
fn cell(line: Option<&DiffLine>, side: Side, colour: Option<Rc<RenderedLine>>, path: &str) -> Element {
    let Some(line) = line else {
        return rsx! { div { class: "diff-cell empty" } };
    };
    let (class, mark) = match line.kind {
        LineKind::Addition => ("diff-cell add", "+"),
        LineKind::Deletion => ("diff-cell del", "−"),
        LineKind::Context => ("diff-cell ctx", " "),
    };
    let number = if side == Side::Old { line.old_number } else { line.new_number };
    rsx! {
        div { class: "{class}",
            span { class: "diff-num", {number.map(|number| number.to_string()).unwrap_or_default()} }
            span { class: "diff-mark", "aria-hidden": "true", "{mark}" }
            { code(line, colour) }
            // A split row has a comment button only in a view that takes line comments of its own.
            if let (Some(notes), Some(number)) = (line_notes().filter(|notes| notes.enabled), number) {
                button { r#type: "button", class: "diff-comment", "aria-label": "Comment on {path} line {number}", title: "Comment on this line",
                    onclick: { let (path, line, begin) = (path.to_owned(), line.clone(), notes.begin); move |_| begin.call((path.clone(), line.clone(), side == Side::New)) }, "+" }
            }
        }
    }
}

/// A stacked row is its own component so "show more" re-renders only the rows it adds.
#[derive(Props, Clone)]
struct StackedRowProps {
    line: DiffLine,
    colour: Option<Rc<RenderedLine>>,
    path: String,
}
impl PartialEq for StackedRowProps {
    fn eq(&self, other: &Self) -> bool {
        self.line == other.line && self.path == other.path && self.colour.as_ref().map(Rc::as_ptr) == other.colour.as_ref().map(Rc::as_ptr)
    }
}
#[allow(non_snake_case)]
fn StackedRow(props: StackedRowProps) -> Element {
    let line = &props.line;
    let (class, mark) = match line.kind {
        LineKind::Addition => ("diff-row add", "+"),
        LineKind::Deletion => ("diff-row del", "−"),
        LineKind::Context => ("diff-row ctx", " "),
    };
    rsx! {
        div { class: "{class}",
            span { class: "diff-num", {line.old_number.map(|number| number.to_string()).unwrap_or_default()} }
            span { class: "diff-num", {line.new_number.map(|number| number.to_string()).unwrap_or_default()} }
            span { class: "diff-mark", "aria-hidden": "true", "{mark}" }
            { code(line, props.colour.clone()) }
            if let (Some(number), true) = (line.new_number.or(line.old_number), line_notes().is_none_or(|notes| notes.enabled)) {
                button { r#type: "button", class: "diff-comment", "aria-label": "Comment on {props.path} line {number}", title: "Comment on this line", onclick: { let (path, line) = (props.path.clone(), line.clone()); move |_| {
                    if let Some(notes) = line_notes() { notes.begin.call((path.clone(), line.clone(), true)); }
                    else if let Some(commenter) = try_consume_context::<Commenter>() { let mut target = commenter.0; target.set(Some(CommentTarget { path: path.clone(), line: line.clone() })); } } }, "+" }
            }
        }
    }
}

/// The id and title of the part of the diff a comment is made in.
fn section_of(scope: &Scope) -> (String, String) {
    match scope {
        Scope::Changes => ("diff:changes".into(), "Changes".into()),
        Scope::Uncommitted => ("diff:uncommitted".into(), "Uncommitted".into()),
        Scope::Latest => ("diff:latest".into(), "Latest turn".into()),
        Scope::Turn(id) => (format!("diff:turn:{id}"), "Turn".into()),
        Scope::Thread => ("diff:thread".into(), "Thread".into()),
    }
}

/// The line of a diff a comment is being written on.
#[derive(Debug, Clone, PartialEq)]
struct CommentTarget {
    path: String,
    line: DiffLine,
}
/// Lets a row ask the panel for a comment dialog without the rows depending on the panel.
#[derive(Clone, Copy)]
struct Commenter(Signal<Option<CommentTarget>>);

/// What a view hangs on the lines of a diff of its own: the pull request panel's review comments. `under` gives the notes
/// that sit below a line (its path, the line, and for one half of a split row whether it is the new half), `begin` is what the line's comment
/// button asks for, and a view that cannot take a comment turns the button off.
#[derive(Clone)]
pub(crate) struct LineNotes {
    pub(crate) enabled: bool,
    pub(crate) under: Rc<dyn Fn(&str, &DiffLine, Option<bool>) -> Option<Element>>,
    pub(crate) begin: Callback<(String, DiffLine, bool)>,
}
fn line_notes() -> Option<LineNotes> {
    try_consume_context::<LineNotes>()
}

/// Writes a comment on one line of a diff and attaches it to the open draft as a chip.
#[component]
fn CommentDialog(state: Store<UiModel>, environment_key: String, thread_id: String, section: (String, String), target: CommentTarget, onclose: EventHandler<()>) -> Element {
    let mut text = use_signal(String::new);
    let number = target.line.new_number.or(target.line.old_number).unwrap_or(1);
    let deleted = target.line.kind == LineKind::Deletion;
    let title = format!("Comment on {} line {}{}", target.path, number, if deleted { " (before)" } else { "" });
    let add = {
        let (target, section, thread_id, environment_key) = (target.clone(), section.clone(), thread_id.clone(), environment_key.clone());
        move || {
            let comment = t3_client::composer_context::line_comment(&format!("review-comment-{}", uuid::Uuid::new_v4().simple()), &section.0, &section.1, &target.path, if target.line.kind == LineKind::Deletion { '-' } else { '+' }, target.line.new_number.or(target.line.old_number).unwrap_or(1), &target.line.text, &text.peek());
            if comment["text"].as_str().is_some_and(str::is_empty) {
                return;
            }
            let record = t3_client::composer_context::review_record(&comment);
            crate::composer_chips::add(state, t3_client::draft_storage::DraftTarget::thread(environment_key.clone(), thread_id.clone()), record);
            onclose.call(());
        }
    };
    rsx! { div { class: "modal-backdrop", onclick: move |_| onclose.call(()),
        div { class: "dialog review-comment-dialog", role: "dialog", "aria-modal": "true", "aria-label": "{title}", tabindex: "-1", onclick: move |event| event.stop_propagation(),
            onkeydown: move |event| { if event.key() == Key::Escape { event.stop_propagation(); onclose.call(()); } },
            h2 { "{title}" }
            pre { class: "review-comment-line", "{target.line.text}" }
            textarea { "aria-label": "Review comment", placeholder: "What should change here?", value: "{text}", oninput: move |event| text.set(event.value()),
                onmounted: move |event| { let element = event.data(); spawn(async move { let _ = element.set_focus(true).await; }); } }
            div { class: "dialog-actions",
                button { r#type: "button", "aria-label": "Cancel review comment", onclick: move |_| onclose.call(()), "Cancel" }
                button { class: "primary", r#type: "button", "aria-label": "Add comment to chat", disabled: text.read().trim().is_empty(), onclick: { let mut add = add.clone(); move |_| add() }, "Add to chat" }
            }
        }
    } }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapse_follows_the_setting_until_the_reader_chooses_and_resets_per_selection() {
        let turn = |id: &str| Scope::Turn(id.to_owned());
        let mut collapse = Collapse::default();
        // Nothing chosen: the client setting rules, whichever selection is showing.
        assert!(collapse.collapsed(&turn("a"), true, "f.rs"));
        assert!(!collapse.collapsed(&turn("a"), false, "f.rs"));
        collapse.for_scope(&turn("a")).overrides.insert("f.rs".into(), false);
        assert!(!collapse.collapsed(&turn("a"), true, "f.rs"));
        assert!(collapse.collapsed(&turn("a"), true, "g.rs"));
        // Another turn starts over from the setting.
        assert!(collapse.collapsed(&turn("b"), true, "f.rs"));
        collapse.for_scope(&turn("b")).default = Some(false);
        assert!(!collapse.collapsed(&turn("b"), true, "g.rs"));
        assert!(collapse.collapsed(&turn("a"), true, "f.rs"), "the first turn's choices were dropped with it");
        // "Expand all" keeps single files closable.
        let edit = collapse.for_scope(&turn("b"));
        edit.overrides.insert("g.rs".into(), true);
        assert!(collapse.collapsed(&turn("b"), false, "g.rs") && !collapse.collapsed(&turn("b"), true, "h.rs"));
    }

    #[test]
    fn answers_for_another_diff_are_rejected() {
        let request = Request { method: checkpoints::TURN_DIFF, payload: serde_json::json!({"threadId":"t","fromTurnCount":1,"toTurnCount":2}), key: "k".into() };
        let answer = |thread: &str, from: u64, to: u64, diff: &str| serde_json::json!({"threadId":thread,"fromTurnCount":from,"toTurnCount":to,"diff":diff});
        assert!(decode(answer("t", 1, 2, ""), "t", &request).is_ok());
        assert!(decode(answer("other", 1, 2, ""), "t", &request).is_err());
        assert!(decode(answer("t", 1, 3, ""), "t", &request).is_err());
        assert!(decode(answer("t", 0, 2, ""), "t", &request).is_err());
        assert!(decode(serde_json::json!({"nonsense":true}), "t", &request).is_err());
        let loaded = decode(answer("t", 1, 2, "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n"), "t", &request).unwrap();
        assert_eq!((loaded.stat, loaded.changed.len()), (Stat { additions: 1, deletions: 1 }, 1));
        assert_eq!(loaded.changed[0].path, "x");
        assert!(matches!(decode(answer("t", 1, 2, "plain words"), "t", &request).unwrap().content, Content::Raw { .. }));
        assert!(matches!(decode(answer("t", 1, 2, "  \n"), "t", &request).unwrap().content, Content::Empty));
    }
}
