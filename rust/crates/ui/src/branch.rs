//! The branch and working-tree status of the open thread's workspace, in the workspace header:
//! the branch it is on, whether it has uncommitted changes, how far it is from its upstream, the
//! ref picker that switches or creates a branch, pull, and "Initialize Git" for a folder that is
//! not a repository. Port of `BranchToolbarBranchSelector`, `BranchPicker` and the pull/init
//! parts of `GitActionsControl`.
//!
//! Everything here is local to the control: the status stream, the picker and its search box
//! write signals owned by these components, so a status update or a keystroke in the picker
//! never repaints the conversation or the sidebar. The stream is held only while the control is
//! mounted, and the server releases its remote-refresh poller when the stream is dropped.
use crate::{
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
};
use dioxus::prelude::*;
use serde_json::{Value, json};
use t3_client::{
    connection::ConnectionStatus,
    vcs::{self, Item, Label, PullOffer, Ref, Refs, Status},
};
use t3_contracts::AuthEnvironmentScope;

/// What the stream has told the control. `live` is false whenever the answer may be out of date:
/// before the first snapshot, while reconnecting, and after the connection is lost. The last
/// status stays for display, marked as such.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct View {
    pub(crate) status: Option<Status>,
    pub(crate) live: bool,
    pub(crate) problem: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
enum Notice {
    Done(String),
    Failed(String),
}

/// The write actions run one at a time, and say what they are doing.
fn writer(state: Store<UiModel>) -> Memo<bool> {
    use_memo(move || {
        let destination = state.destination().read().clone();
        *state.status().read() == ConnectionStatus::Connected
            && destination.is_some_and(|destination| {
                let field = state.environments();
                let environments = field.read();
                environments.allows(&destination, AuthEnvironmentScope::SourceControlWrite)
                    && environments.allows(&destination, AuthEnvironmentScope::OrchestrationOperate)
            })
    })
}

/// Asks the server to look at the workspace again; the stream carries what it finds.
fn refresh(transport: &TransportHandle, state: Store<UiModel>, cwd: &str) {
    let (transport, cwd) = (transport.clone(), cwd.to_owned());
    dioxus::dioxus_core::spawn_forever(async move {
        let _ = runtime::request_value_reported(transport, state, vcs::REFRESH_STATUS, json!({"cwd":cwd})).await;
    });
}

#[component]
pub(crate) fn Control(
    state: Store<UiModel>,
    transport: TransportHandle,
    thread_id: String,
    project_id: String,
    /// Where the thread works: its worktree, else the project's checkout.
    cwd: String,
    project_cwd: String,
    worktree: Option<String>,
) -> Element {
    let mut view = use_signal(View::default);
    let mut busy = use_signal(|| None::<String>);
    let mut notice = use_signal(|| None::<Notice>);
    let mut picker = use_signal(|| false);
    // Whether this connection may read the workspace's status at all, and whether it is
    // connected right now. Without the grant the control is absent; without the connection it
    // keeps the last status it had, marked as old.
    let permitted = use_memo(move || {
        let destination = state.destination().read().clone();
        destination.is_some_and(|destination| state.environments().read().allows(&destination, AuthEnvironmentScope::OrchestrationRead))
    });
    let connected = use_memo(move || *state.status().read() == ConnectionStatus::Connected);
    let can_read = use_memo(move || permitted() && connected());
    let can_write = writer(state);

    // The status stream: one subscription for this workspace while it is on screen and the
    // connection can read it. The resource restarts, and drops the stream, when either changes.
    {
        let (transport, cwd) = (transport.clone(), cwd.clone());
        use_resource(move || {
            let (transport, cwd, readable) = (transport.clone(), cwd.clone(), can_read());
            async move {
                if view.peek().live {
                    view.write().live = false;
                }
                if !readable {
                    return;
                }
                let mut wait = 500;
                loop {
                    // Without a connection there is nothing to ask; the resource starts over when
                    // one returns, instead of this loop failing against a closed socket.
                    if *state.status().peek() != ConnectionStatus::Connected {
                        return;
                    }
                    match runtime::request_stream(&transport, state, vcs::SUBSCRIBE_STATUS, json!({"cwd":cwd,"includeRemote":true})) {
                        Err(error) => view.write().problem = Some(error),
                        Ok(mut stream) => {
                            let mut current: Option<Status> = None;
                            while let Some(result) = stream.next().await {
                                let values = match result {
                                    Ok(values) => values,
                                    Err(error) => {
                                        view.write().problem = Some(error);
                                        break;
                                    }
                                };
                                for value in values {
                                    match Status::apply(current.as_ref(), &value) {
                                        Ok(status) => current = Some(status),
                                        Err(error) => view.write().problem = Some(error),
                                    }
                                }
                                if let Some(status) = current.clone() {
                                    wait = 500;
                                    *view.write() = View { status: Some(status), live: true, problem: None };
                                }
                            }
                            view.write().live = false;
                        }
                    }
                    runtime::delay(wait).await;
                    wait = (wait * 2).min(5000);
                }
            }
        });
    }

    // Agents change the working tree: look again when a turn ends.
    {
        let (transport, cwd, thread_id) = (transport.clone(), cwd.clone(), thread_id.clone());
        let working = use_memo(move || {
            state.shell().read().snapshot.as_ref().is_some_and(|snapshot| snapshot.threads.iter().any(|thread| thread.id.as_str() == thread_id && thread.status.is_active()))
        });
        let mut was_working = use_signal(|| false);
        use_effect(move || {
            let now = working();
            if *was_working.peek() && !now && can_read() {
                refresh(&transport, state, &cwd);
            }
            was_working.set(now);
        });
    }

    // Every action says what it is doing, runs alone, and reports where it finished. The label
    // never moves until the server's status does, so a failed switch leaves the old branch.
    let act = {
        let (transport, cwd) = (transport.clone(), cwd.clone());
        use_callback(move |(doing, method, payload, title, metadata): (String, &'static str, Value, &'static str, Option<Value>)| {
            if busy.peek().is_some() {
                return;
            }
            busy.set(Some(doing));
            notice.set(None);
            let (transport, cwd) = (transport.clone(), cwd.clone());
            spawn(async move {
                let owner = runtime::response_owner(&transport, state);
                let result = runtime::request_value_reported(transport.clone(), state, method, payload).await;
                if owner.is_none() || owner != runtime::response_owner(&transport, state) {
                    busy.set(None);
                    return;
                }
                match result {
                    Ok(value) => {
                        if let Some(mut metadata) = metadata {
                            // The thread records the branch it works on, as the original does.
                            if let Some(name) = value["refName"].as_str() {
                                metadata["branch"] = json!(name);
                            }
                            runtime::command(&transport, state, "thread.metadata.update", metadata);
                        }
                        if method == vcs::PULL {
                            let (heading, detail) = vcs::pull_outcome(&value);
                            notice.set(Some(Notice::Done(format!("{heading}. {detail}"))));
                        }
                        refresh(&transport, state, &cwd);
                    }
                    Err(message) => notice.set(Some(Notice::Failed(format!("{title} {message}")))),
                }
                busy.set(None);
            });
        })
    };

    let pick = {
        let (project_cwd, worktree, thread_id) = (project_cwd.clone(), worktree.clone(), thread_id.clone());
        let transport = transport.clone();
        use_callback(move |chosen: Ref| {
            picker.set(false);
            let selection = vcs::selection(&project_cwd, worktree.as_deref(), &chosen);
            if selection.reuse_existing_worktree {
                // The branch is already checked out in a worktree: the thread moves there.
                if !chosen.current || selection.next_worktree.as_deref() != worktree.as_deref() {
                    runtime::command(&transport, state, "thread.metadata.update", json!({"threadId":thread_id,"branch":chosen.name,"worktreePath":selection.next_worktree}));
                }
                return;
            }
            if chosen.current {
                return;
            }
            let metadata = json!({"threadId":thread_id,"worktreePath":selection.next_worktree});
            act.call((format!("Switching to {}\u{2026}", chosen.name), vcs::SWITCH_REF, json!({"cwd":selection.checkout_cwd,"refName":chosen.name}), "Failed to switch ref.", Some(metadata)));
        })
    };
    let create = {
        let (cwd, thread_id, worktree) = (cwd.clone(), thread_id.clone(), worktree.clone());
        use_callback(move |name: String| {
            picker.set(false);
            let metadata = json!({"threadId":thread_id,"worktreePath":worktree});
            act.call((format!("Creating {name}\u{2026}"), vcs::CREATE_REF, json!({"cwd":cwd,"refName":name,"switchRef":true}), "Failed to create and switch ref.", Some(metadata)));
        })
    };

    let init = {
        let cwd = cwd.clone();
        use_callback(move |_: ()| act.call(("Initializing\u{2026}".to_owned(), vcs::INIT, json!({"cwd":cwd}), "Git initialization failed.", None)))
    };
    let pull_now = {
        let cwd = cwd.clone();
        use_callback(move |_: ()| act.call(("Pulling\u{2026}".to_owned(), vcs::PULL, json!({"cwd":cwd}), "Pull failed.", None)))
    };
    let open_picker = {
        let (transport, cwd) = (transport.clone(), cwd.clone());
        use_callback(move |_: ()| {
            let open = !picker();
            picker.set(open);
            if open {
                refresh(&transport, state, &cwd);
            }
        })
    };
    // Tells the palette and the shortcut whether the picker can open, and opens it on request.
    use_effect(move || {
        let available = can_write() && !busy.read().is_some() && {
            let snapshot = view.read();
            snapshot.live && matches!(vcs::label(snapshot.status.as_ref()), Label::Detached | Label::Branch(_))
        };
        if state.branch_available().peek().clone() != available {
            state.branch_available().set(available);
        }
    });
    use_drop(move || {
        if *state.branch_available().peek() {
            state.branch_available().set(false);
        }
    });
    let mut seen_toggles = use_signal(|| *state.branch_toggles().peek());
    use_effect(move || {
        let requested = *state.branch_toggles().read();
        if requested != *seen_toggles.peek() {
            seen_toggles.set(requested);
            if *state.branch_available().peek() {
                open_picker.call(());
            }
        }
    });
    if !permitted() {
        return rsx! {};
    }
    let snapshot = view.read().clone();
    let writable = can_write();
    let label = vcs::label(snapshot.status.as_ref());
    let pull = snapshot.status.as_ref().map_or(PullOffer::None, Status::pull);
    let marks = snapshot.status.as_ref().map(Status::marks).unwrap_or_default().join(" ");
    let summary = snapshot.status.as_ref().filter(|status| status.local.has_working_tree_changes).map(Status::changes_summary);
    let busy_now = busy.read().clone();
    let notice_now = notice.read().clone();
    let stale = !snapshot.live;
    let offline_title = snapshot.problem.clone().unwrap_or_else(|| "Not connected. This is the last branch seen here.".to_owned());
    let class = if stale { "branch-control stale" } else { "branch-control" };
    rsx! {
        div { class: "{class}",
            match label {
                Label::Loading => rsx! {
                    if let Some(problem) = snapshot.problem.clone() { span { class: "branch-name", title: "{problem}", "Branch unavailable" } }
                    else { span { class: "branch-name", title: "Checking the branch\u{2026}", "\u{2442} \u{2026}" } }
                },
                Label::NotRepo => rsx! {
                    span { class: "branch-name", "Not a git repository" }
                    if writable && !stale {
                        button { class: "branch-action", "aria-label": "Initialize Git", disabled: busy_now.is_some(),
                            onclick: move |_| init.call(()),
                            if busy_now.is_some() { "Initializing\u{2026}" } else { "Initialize Git" } }
                    }
                },
                Label::Detached | Label::Branch(_) => {
                    let name = match &label { Label::Branch(name) => name.clone(), _ => "Detached HEAD".to_owned() };
                    rsx! {
                        if writable && !stale {
                            button { class: "branch-name branch-trigger", "aria-label": "Switch branch", "aria-haspopup": "listbox", "aria-expanded": "{picker()}", title: "{name}", disabled: busy_now.is_some(),
                                onclick: move |_| open_picker.call(()),
                                "\u{2442} {name}" }
                        } else {
                            span { class: "branch-name", title: "{name}", "\u{2442} {name}" }
                        }
                    }
                }
            }
            if stale && snapshot.status.is_some() {
                span { class: "branch-offline", title: "{offline_title}", "offline" }
            }
            if let Some(summary) = summary { span { class: "branch-marks dirty", title: "{summary}", "aria-label": "{summary}", "{marks}" } }
            else if !marks.is_empty() { span { class: "branch-marks", title: "{marks}", "{marks}" } }
            if let Some(text) = &busy_now { span { class: "branch-busy", role: "status", "{text}" } }
            if matches!(label, Label::Detached | Label::Branch(_)) {
                crate::git_control::Actions { state, transport: transport.clone(), view, thread_id: thread_id.clone(), project_id: project_id.clone(), cwd: cwd.clone(), busy, on_pull: pull_now }
            }
            if pull == PullOffer::Diverged && !stale { span { class: "branch-hint", title: "{vcs::DIVERGED_HINT}", "diverged" } }
            match notice_now {
                Some(Notice::Done(text)) => rsx! { div { class: "branch-notice", role: "status", span { "{text}" } button { "aria-label": "Dismiss branch message", onclick: move |_| notice.set(None), "\u{d7}" } } },
                Some(Notice::Failed(text)) => rsx! { div { class: "branch-notice failure", role: "alert", span { "{text}" } button { "aria-label": "Dismiss branch message", onclick: move |_| notice.set(None), "\u{d7}" } } },
                None => rsx! {},
            }
            if picker() && writable {
                Picker { state, transport: transport.clone(), cwd: cwd.clone(), project_cwd: project_cwd.clone(), allow_create: true, on_pick: pick, on_create: create, on_close: move |_| picker.set(false) }
            }
        }
    }
}

/// What the picker has loaded for one search.
#[derive(Debug, Clone, PartialEq, Default)]
struct Listing {
    /// The search these refs answer, so a late page of another search is dropped.
    settled: String,
    refs: Refs,
    loading: bool,
    loading_next: bool,
    error: Option<String>,
}

fn list_payload(cwd: &str, settled: &str, cursor: Option<u64>) -> Value {
    let mut payload = json!({"cwd":cwd,"limit":vcs::REF_PAGE_LIMIT});
    if !settled.is_empty() {
        payload["query"] = json!(settled);
    }
    if let Some(cursor) = cursor {
        payload["cursor"] = json!(cursor);
    }
    payload
}

/// The ref picker: search, page, choose. `allow_create` adds the "Create new ref" row for what
/// is typed, which only the thread's own branch control wants.
#[component]
pub(crate) fn Picker(
    state: Store<UiModel>,
    transport: TransportHandle,
    cwd: String,
    project_cwd: String,
    allow_create: bool,
    on_pick: Callback<Ref>,
    on_create: Callback<String>,
    on_close: Callback<()>,
) -> Element {
    let mut query = use_signal(String::new);
    let mut settled = use_signal(String::new);
    let mut listing = use_signal(|| Listing { loading: true, ..Listing::default() });
    let mut highlighted = use_signal(|| 0usize);
    let mut previous_top = use_signal(|| None::<f64>);

    // The server is asked once typing pauses; a keystroke before then cancels the wait. The
    // search sent is the sanitized one, because refs cannot contain spaces.
    use_resource(move || async move {
        let typed = vcs::sanitize_new_ref_name(&query());
        if typed != *settled.peek() {
            runtime::delay(vcs::QUERY_DEBOUNCE_MS).await;
            settled.set(typed);
            highlighted.set(0);
        }
    });

    // The first page of the settled search. A new search cancels the one in flight, and an answer
    // that arrives after the connection or the search changed is not shown.
    {
        let (transport, cwd) = (transport.clone(), cwd.clone());
        use_resource(move || {
            let (transport, cwd, search) = (transport.clone(), cwd.clone(), settled());
            async move {
                {
                    let mut now = listing.write();
                    now.loading = true;
                    now.loading_next = false;
                    now.error = None;
                }
                let owner = runtime::response_owner(&transport, state);
                let answer = runtime::request_value_reported(transport.clone(), state, vcs::LIST_REFS, list_payload(&cwd, &search, None)).await;
                if owner.is_none() || owner != runtime::response_owner(&transport, state) {
                    return;
                }
                let mut refs = Refs::default();
                let merged = answer.and_then(|page| refs.merge(&page));
                match merged {
                    Ok(()) => *listing.write() = Listing { settled: search, refs, loading: false, loading_next: false, error: None },
                    Err(error) => {
                        let mut now = listing.write();
                        now.loading = false;
                        now.error = Some(error);
                    }
                }
            }
        });
    }

    let load_next = {
        let (transport, cwd) = (transport.clone(), cwd.clone());
        use_callback(move |_: ()| {
            let (search, cursor) = {
                let now = listing.peek();
                (now.settled.clone(), now.refs.next_cursor)
            };
            let Some(cursor) = cursor else { return };
            if listing.peek().loading || listing.peek().loading_next {
                return;
            }
            listing.write().loading_next = true;
            let (transport, cwd) = (transport.clone(), cwd.clone());
            spawn(async move {
                let owner = runtime::response_owner(&transport, state);
                let answer = runtime::request_value_reported(transport.clone(), state, vcs::LIST_REFS, list_payload(&cwd, &search, Some(cursor))).await;
                if owner.is_none() || owner != runtime::response_owner(&transport, state) {
                    return;
                }
                let mut now = listing.write();
                // The list may have moved on to another search while this page was coming.
                if now.settled != search || now.refs.next_cursor != Some(cursor) {
                    now.loading_next = false;
                    return;
                }
                now.loading_next = false;
                if let Err(error) = answer.and_then(|page| now.refs.merge(&page)) {
                    now.error = Some(error);
                }
            });
        })
    };

    let snapshot = listing.read().clone();
    let typed = query.read().clone();
    let rows = vcs::items(&snapshot.refs, &typed, &snapshot.settled, allow_create);
    let selected = (*highlighted.read()).min(rows.len().saturating_sub(1));
    let status = if snapshot.loading && snapshot.refs.refs.is_empty() { Some("Loading refs...".to_owned()) } else { snapshot.refs.status_text(snapshot.loading_next) };
    let more = snapshot.refs.next_cursor.is_some() && !snapshot.loading_next;
    let count = rows.len();
    let choose = {
        let rows = rows.clone();
        use_callback(move |index: usize| match rows.get(index) {
            Some(Item::Ref(chosen)) => on_pick.call(chosen.clone()),
            Some(Item::Create(name)) => on_create.call(name.clone()),
            None => {}
        })
    };
    rsx! {
        div { class: "branch-picker-backdrop", onclick: move |_| on_close.call(()) }
        div { class: "branch-picker", role: "dialog", "aria-label": "Switch branch",
            input { class: "branch-search", "aria-label": "Search refs", placeholder: "Search refs...", autocomplete: "off", value: "{typed}",
                onmounted: move |event| { let element = event.data(); spawn(async move { let _ = element.set_focus(true).await; }); },
                oninput: move |event| query.set(event.value()),
                onkeydown: move |event| match event.key() {
                    Key::ArrowDown => { event.prevent_default(); highlighted.set((selected + 1).min(count.saturating_sub(1))); }
                    Key::ArrowUp => { event.prevent_default(); highlighted.set(selected.saturating_sub(1)); }
                    Key::Enter => { event.prevent_default(); choose.call(selected); }
                    Key::Escape => on_close.call(()),
                    _ => {}
                } }
            div { class: "branch-list", role: "listbox", "aria-label": "Refs",
                onscroll: move |event| {
                    let data = event.data();
                    let top = data.scroll_top();
                    let near_end = vcs::should_load_next(*previous_top.peek(), top, f64::from(data.scroll_height()), f64::from(data.client_height()));
                    previous_top.set(Some(top));
                    if near_end {
                        load_next.call(());
                    }
                },
                for (index, row) in rows.iter().enumerate() {
                    match row {
                        Item::Ref(chosen) => rsx! {
                            button { key: "ref:{chosen.name}", class: if index == selected { "branch-row highlighted" } else { "branch-row" }, role: "option", "aria-selected": "{chosen.current}", "aria-label": "{chosen.name}",
                                onclick: { let chosen = chosen.clone(); move |_| on_pick.call(chosen.clone()) },
                                span { class: "branch-row-name", "{chosen.name}" }
                                if let Some(badge) = chosen.badge(Some(&project_cwd)) { span { class: "branch-row-badge", "{badge}" } }
                            }
                        },
                        Item::Create(name) => rsx! {
                            button { key: "create:{name}", class: if index == selected { "branch-row highlighted" } else { "branch-row" }, role: "option", "aria-selected": "false", "aria-label": "Create new ref \"{name}\"",
                                onclick: { let name = name.clone(); move |_| on_create.call(name.clone()) },
                                span { class: "branch-row-name", "Create new ref \"{name}\"" }
                            }
                        },
                    }
                }
                if rows.is_empty() && !snapshot.loading && snapshot.error.is_none() { p { class: "muted branch-empty", "No refs found." } }
            }
            if let Some(error) = snapshot.error { p { class: "branch-picker-error", role: "alert", "{error}" } }
            if let Some(text) = status {
                if more {
                    button { class: "branch-more", "aria-label": "Load more refs", onclick: move |_| load_next.call(()), "{text}" }
                } else {
                    p { class: "muted branch-status", role: "status", "{text}" }
                }
            }
        }
    }
}

/// "Browse" beside the new-thread form's base branch: the same picker over the project's checkout,
/// choosing a ref instead of switching to it. Reading refs needs only the read grant.
#[component]
pub(crate) fn BaseBranch(state: Store<UiModel>, transport: TransportHandle, cwd: String, on_pick: Callback<Ref>) -> Element {
    let mut open = use_signal(|| false);
    let can_read = use_memo(move || {
        let destination = state.destination().read().clone();
        *state.status().read() == ConnectionStatus::Connected && destination.is_some_and(|destination| state.environments().read().allows(&destination, AuthEnvironmentScope::OrchestrationRead))
    });
    rsx! {
        div { class: "branch-control base-branch",
            button { r#type: "button", class: "branch-action", "aria-label": "Choose base branch", "aria-haspopup": "listbox", "aria-expanded": "{open()}", disabled: !can_read(), onclick: move |_| open.set(!open()), "Choose branch" }
            if open() && can_read() {
                Picker { state, transport, cwd: cwd.clone(), project_cwd: cwd, allow_create: false,
                    on_pick: move |chosen: Ref| { open.set(false); on_pick.call(chosen); },
                    on_create: |_| {}, on_close: move |_| open.set(false) }
            }
        }
    }
}
