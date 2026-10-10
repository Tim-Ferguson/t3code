//! A thread in the sidebar: its state at a glance, and the menu of what can be done to it. The row
//! takes plain props, so a status change on one thread repaints that row alone. Status, unread and
//! snooze come from the thread shell the server sends, so after a reconnect they are what the
//! server now says. Port of the thread row of `Sidebar.tsx` and `threadActionMenu.logic.ts`.
use crate::{
    notice::{self, Level},
    runtime::{self, UiModel, UiModelStoreExt},
    thread_lifecycle,
};
use chrono::Utc;
use dioxus::prelude::*;
use serde_json::{Value, json};
use t3_client::sidebar::{self, Pill, Status};
use t3_contracts::ThreadShell;

/// What the server says it can do for threads. Each action is offered only where it is.
#[derive(Clone, Copy, PartialEq, Default)]
pub(crate) struct Caps {
    pub pinning: bool,
    /// A pin can be moved: the server stores a pin order.
    pub pin_reorder: bool,
    /// An active thread can be moved: the server stores an active order.
    pub active_reorder: bool,
    pub snooze: bool,
    /// `thread.settle` is understood; without it no thread is ever shown as settled.
    pub settlement: bool,
    /// `thread.auto-settle.set` is understood: a thread can opt out of automatic settling.
    pub auto_settle_opt_out: bool,
    pub title_regeneration: bool,
}
impl Caps {
    pub fn support(self) -> sidebar::Support {
        sidebar::Support { settlement: self.settlement, snooze: self.snooze }
    }
}
pub(crate) fn caps(state: Store<UiModel>) -> Caps {
    caps_of(state)
}
fn caps_of(state: Store<UiModel>) -> Caps {
    let field = state.typed_config();
    let config = field.read();
    let capabilities = config.as_ref().map(|config| &config.environment.capabilities);
    Caps {
        pinning: capabilities.is_some_and(|c| c.thread_pinning == Some(true)),
        pin_reorder: capabilities.is_some_and(|c| c.thread_pin_reorder == Some(true)),
        active_reorder: capabilities.is_some_and(|c| c.thread_active_reorder == Some(true)),
        settlement: capabilities.is_some_and(|c| c.thread_settlement == Some(true)),
        auto_settle_opt_out: capabilities.is_some_and(|c| c.thread_auto_settle_opt_out == Some(true)),
        snooze: capabilities.is_some_and(|c| c.thread_snooze == Some(true)),
        title_regeneration: capabilities.is_some_and(|c| c.thread_title_regeneration == Some(true)),
    }
}

#[derive(Clone, PartialEq)]
pub(crate) struct RowView {
    pub id: String,
    pub project_id: String,
    pub title: String,
    pub selected: bool,
    pub status: Status,
    pub pill: Option<Pill>,
    pub unread: bool,
    pub pinned: bool,
    /// "2h" while snoozed.
    pub snoozed: Option<String>,
    /// When a snooze that ended on its timer woke the thread, until it is visited or dismissed.
    pub woke_at: Option<String>,
    pub regenerating: bool,
    /// Automatic settling is on for this thread (the user has not opted it out).
    pub auto_settle: bool,
    pub working_since: Option<String>,
    /// "Goal" while a native goal keeps the agent going, else "Working" (`thread.goal?.status === "active"`).
    pub working_label: &'static str,
    /// How long ago the thread was written to, or for a settled one how long since it wrapped up.
    pub time: String,
    /// The provider instances that owned the thread before the one it has now, oldest first, by name.
    pub previous_providers: Vec<String>,
    pub pull_request: Option<PullRequestBadge>,
    pub branch: Option<String>,
    pub workspace: Option<String>,
    /// The thread's own worktree, which "New thread on <branch>" reuses.
    pub worktree: Option<String>,
    pub has_completed_run: bool,
    pub can_snooze: bool,
    pub can_operate: bool,
    pub caps: Caps,
    pub section: sidebar::Section,
    /// The sections a drag of this row may land in, space separated; empty when it cannot be dragged.
    pub drops: String,
}

/// What a row says of the thread's pull requests (`ThreadPullRequestBadgeControl`): the glyph of their state, the number (or
/// the layers of a stack, or `+N` for several), and what a click opens.
#[derive(Clone, PartialEq)]
pub(crate) struct PullRequestBadge {
    pub text: String,
    /// The accessible name and tooltip.
    pub label: String,
    pub state: t3_client::thread_pull_requests::State,
    pub stack: bool,
    /// A stack or several links open the thread's list; one request opens itself.
    pub opens_list: bool,
    pub target: Option<crate::pull_request_panel::Target>,
    pub url: String,
}

/// `resolveThreadPullRequestBadgePresentation`: none for a thread with nothing linked.
fn pull_request_badge(thread: &ThreadShell) -> Option<PullRequestBadge> {
    use t3_client::thread_pull_requests as links;
    let linked = thread.pull_requests.as_ref().and_then(Option::as_ref).is_some_and(|list| !list.is_empty()) || thread.linked_pull_request.as_ref().is_some_and(Option::is_some);
    if !linked {
        return None;
    }
    let value = serde_json::to_value(thread).ok()?;
    let badge = links::badge(&value);
    let current = links::current(&value).or_else(|| links::links(&value).into_iter().next());
    if let Some(links::Badge::Stack { layers, state }) = badge {
        let target = current.as_ref().map(|link| crate::pull_request_panel::Target { thread_id: thread.id.to_string(), reference: link.reference(thread.project_id.as_str()) });
        return Some(PullRequestBadge { text: layers.to_string(), label: format!("Stack of {layers} pull requests, {}", state.label().to_lowercase()), state, stack: true, opens_list: true, target, url: current.map(|link| link.url).unwrap_or_default() });
    }
    let link = current?;
    let state = links::state_of(&value, &link);
    let name = if link.url.contains("/-/merge_requests/") { "MR" } else { "PR" };
    let tooltip = match link.title.as_deref() {
        Some(title) => format!("{name} #{} - {}: {title}", link.number, state.label()),
        None => format!("{name} #{}, status pending", link.number),
    };
    let target = Some(crate::pull_request_panel::Target { thread_id: thread.id.to_string(), reference: link.reference(thread.project_id.as_str()) });
    Some(match badge {
        Some(links::Badge::PullRequest { others, state: overall }) if others > 0 => PullRequestBadge { text: format!("+{}", others + 1), label: format!("{tooltip}, and {others} more linked; overall {}", overall.label().to_lowercase()), state: overall, stack: false, opens_list: true, target, url: link.url },
        _ => PullRequestBadge { text: link.number.to_string(), label: tooltip, state, stack: false, opens_list: false, target, url: link.url },
    })
}

/// The threads of a project the sidebar lists: not removed, archived or a subagent's.
pub(crate) fn listed<'a>(threads: &'a [ThreadShell], project_id: &'a str) -> impl Iterator<Item = &'a ThreadShell> {
    threads.iter().filter(move |thread| thread.project_id.as_str() == project_id && thread.deleted_at.is_none() && thread.archived_at.is_none() && thread.lineage.relationship_to_parent != Some(t3_contracts::ThreadRelationship::Subagent))
}
/// A project's threads in the order the sidebar lists them, whatever is collapsed: pinned, active,
/// snoozed, settled.
pub(crate) fn rows<'a>(threads: &'a [ThreadShell], project_id: &'a str, now: chrono::DateTime<Utc>, support: sidebar::Support) -> Vec<&'a ThreadShell> {
    let sections = sidebar::Sections::of(listed(threads, project_id), now, support);
    [sections.pinned, sections.active, sections.snoozed, sections.settled].concat()
}

/// The names the provider stack of a row is drawn with, by instance id.
pub(crate) fn provider_names(state: Store<UiModel>) -> std::collections::BTreeMap<String, String> {
    let field = state.typed_config();
    let config = field.read();
    config.as_ref().map(|config| config.providers.0.iter().map(|provider| (provider.instance_id.to_string(), provider.display_name.clone().flatten().map_or_else(|| provider.instance_id.to_string(), |name| name.to_string()))).collect()).unwrap_or_default()
}

pub(crate) fn view(thread: &ThreadShell, selected: bool, workspace: Option<String>, caps: Caps, can_operate: bool, now: chrono::DateTime<Utc>, providers: &std::collections::BTreeMap<String, String>) -> RowView {
    let snoozed = sidebar::snoozed_until(thread, now);
    let section = sidebar::section_of(thread, now, caps.support());
    // A drag needs the server to store an order (as the original's rows do); each landing place
    // needs its own capability. Snoozed shelves are never a landing place: a snooze needs a wake time.
    let drops = if can_operate && caps.pinning && caps.pin_reorder {
        [Some("pinned"), caps.active_reorder.then_some("active"), (caps.settlement && section != sidebar::Section::Settled).then_some("settled")].into_iter().flatten().collect::<Vec<_>>().join(" ")
    } else {
        String::new()
    };
    let working = sidebar::status(thread) == Status::Working;
    RowView {
        id: thread.id.to_string(),
        project_id: thread.project_id.to_string(),
        title: thread.title.clone(),
        selected,
        status: sidebar::status(thread),
        pill: sidebar::pill(thread),
        unread: sidebar::has_unseen_completion(thread),
        pinned: sidebar::is_pinned(thread),
        snoozed: snoozed.filter(|_| section == sidebar::Section::Snoozed).map(|wake| sidebar::wake_label(&wake.to_rfc3339(), now)),
        woke_at: sidebar::woke_at(thread, now).map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
        regenerating: matches!(thread.title_regeneration, Some(Some(_))),
        auto_settle: !matches!(thread.auto_settle_disabled_at, Some(Some(_))),
        working_label: if thread.goal.clone().flatten().is_some_and(|goal| goal.status == t3_contracts::ProviderGoalStatus::Active) { "Goal" } else { "Working" },
        time: if section == sidebar::Section::Settled { sidebar::settled_time_label(thread, now) } else { sidebar::time_label(thread, now) },
        previous_providers: thread.provider_instance_history.iter().filter(|instance| **instance != if sidebar::runtime_status(thread).is_some() { thread.provider_instance_id.clone() } else { thread.model_selection.instance_id.clone() }).map(|instance| providers.get(instance.as_str()).cloned().unwrap_or_else(|| instance.to_string())).collect(),
        pull_request: pull_request_badge(thread),
        working_since: working.then(|| thread.activity_run_started_at.flatten().map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))).flatten(),
        branch: thread.branch.as_ref().map(ToString::to_string),
        workspace,
        worktree: thread.worktree_path.as_ref().map(ToString::to_string),
        has_completed_run: thread.latest_run_id.is_some() && !thread.status.is_active(),
        can_snooze: thread.pending_runtime_request.is_none(),
        can_operate,
        caps,
        section,
        drops,
    }
}


/// "Working for 3m" beside a working thread; the only part of a row that ticks.
#[component]
fn WorkingFor(since: String, label: &'static str) -> Element {
    let mut now = use_signal(|| Utc::now().timestamp_millis());
    use_future(move || async move {
        loop {
            runtime::delay(1000).await;
            now.set(Utc::now().timestamp_millis());
        }
    });
    let started = chrono::DateTime::parse_from_rfc3339(&since).map(|at| at.timestamp_millis()).unwrap_or_else(|_| now());
    rsx! {span {class:"thread-duration","{label} {sidebar::working_label(now() - started)}"}}
}

/// Puts text on the clipboard from the click that asked for it.
pub(crate) fn copy_text(text: &str) {
    let text = serde_json::to_string(text).expect("clipboard text");
    dioxus::document::eval(&format!("navigator.clipboard?.writeText({text});"));
}

/// The thread commands a row sends, one request per click, answered or failed in its own words.
pub(crate) fn send(transport: &runtime::TransportHandle, state: Store<UiModel>, mut busy: Signal<bool>, kind: &'static str, mut payload: Value, failure: &'static str) {
    if *busy.peek() {
        return;
    }
    busy.set(true);
    payload["type"] = json!(kind);
    payload["commandId"] = json!(uuid::Uuid::new_v4().to_string());
    let transport = transport.clone();
    spawn(async move {
        let owner = runtime::response_owner(&transport, state);
        let result = runtime::request_value_reported(transport.clone(), state, "orchestration.dispatchCommand", payload).await;
        busy.set(false);
        if owner.is_none() || owner != runtime::response_owner(&transport, state) {
            return;
        }
        if let Err(reason) = result {
            notice::push(state, Level::Error, failure, reason);
        }
    });
}

#[component]
pub(crate) fn ThreadRow(view: RowView, state: Store<UiModel>, transport: runtime::TransportHandle) -> Element {
    #[cfg(test)]
    if let Some(probe) = try_consume_context::<crate::RenderProbe>() {
        probe.sidebar_row.set(probe.sidebar_row.get() + 1);
    }
    let mut menu = use_signal(|| None::<(f64, f64)>);
    let busy = use_signal(|| false);
    // The title while it is being edited in place, and whether the edit has already been settled
    // (Enter or Escape is followed by the blur the removed field raises).
    let mut renaming = use_signal(|| None::<String>);
    let mut settled = use_signal(|| false);
    let id = view.id.clone();
    let status = view.status.as_str();
    // The words the original's row says (`topStatus` of `Sidebar.tsx`), which are not the words of a status pill.
    let label: Option<(&'static str, &'static str)> = match view.status {
        Status::Approval => Some(("Approval", "amber")),
        Status::Input => Some(("Input", "indigo")),
        Status::Waiting => Some(("Waiting", "muted")),
        Status::Working => view.working_since.is_none().then_some((view.working_label, "sky")),
        Status::Failed | Status::Limited => None,
        Status::Ready => view.unread.then_some(("Done", "emerald")),
    };
    let class = format!("thread-row{}{}{}", if view.selected { " selected" } else { "" }, if view.snoozed.is_some() { " snoozed" } else { "" }, if view.unread { " unread" } else { "" });
    let mut close = move || menu.set(None);
    let item = |name: String, disabled: bool, action: Box<dyn Fn()>| {
        let disabled = disabled || busy();
        rsx! {button {class:"menu-item",role:"menuitem",r#type:"button",disabled,"aria-label":"{name}",onclick:move |_| {close(); action();},"{name}"}}
    };
    let can = view.can_operate;
    let title_now = view.title.clone();
    let (transport_for_menu, view_for_menu) = (transport.clone(), view.clone());
    let items = if menu.read().is_some() {
        let (transport, view) = (transport_for_menu, view_for_menu);
        let thread_command = {
            let (transport, id) = (transport.clone(), view.id.clone());
            move |kind: &'static str, extra: Value, failure: &'static str| {
                let mut payload = extra;
                payload["threadId"] = json!(id);
                send(&transport, state, busy, kind, payload, failure);
            }
        };
        let mut entries: Vec<Element> = vec![];
        // Another thread's history, attached to the draft being written so the agent can read it.
        {
            let (thread_id, title) = (view.id.clone(), view.title.clone());
            let target = crate::composer_chips::open_target(state);
            let environment = state.destination().peek().as_ref().map(ToString::to_string).unwrap_or_default();
            let own = target.as_ref().is_some_and(|target| target.local_id == thread_id);
            entries.push(item("Reference in chat".to_owned(), target.is_none() || own, Box::new(move || {
                if let Some(target) = target.clone() {
                    crate::composer_chips::add(state, target, t3_client::composer_context::thread_record(&environment, &thread_id, &title));
                }
            })));
        }
        // Where a thread sits among its section: one key written to it, or the section's keys when a neighbour has none.
        // The keyboard path of the drag; only the pinned and the active sections are arranged.
        for (section, writable, up_name, down_name, kind) in [(sidebar::Section::Pinned, view.caps.pinning && view.caps.pin_reorder, "Move pin up", "Move pin down", "thread.pin.reorder"), (sidebar::Section::Active, view.caps.active_reorder, "Move thread up", "Move thread down", "thread.active.reorder")] {
            if view.section != section || !writable {
                continue;
            }
            let (order, keys) = section_order(state, &view.id, section);
            for (up, name) in [(true, up_name), (false, down_name)] {
                let next = t3_client::pin_order::moved(&order, &view.id, up);
                let (transport, id) = (transport.clone(), view.id.clone());
                let keys = keys.clone();
                entries.push(item(name.to_owned(), !can || next.is_none(), Box::new(move || {
                    let Some(order) = next.clone() else { return };
                    for (target, key) in t3_client::pin_order::plan_reorder(&order, &keys, &id) {
                        runtime::command(&transport, state, kind, json!({"threadId":target,"orderKey":key}));
                    }
                })));
            }
        }
        if view.caps.pinning {
            let command = thread_command.clone();
            entries.push(if view.pinned { item("Unpin thread".to_owned(), !can, Box::new(move || command("thread.unpin", json!({}), "Failed to unpin thread"))) } else { item("Pin thread".to_owned(), !can, Box::new(move || command("thread.pin", json!({}), "Failed to pin thread"))) });
        }
        if view.caps.settlement {
            let command = thread_command.clone();
            entries.push(if view.section == sidebar::Section::Settled {
                item("Un-settle thread".to_owned(), !can, Box::new(move || command("thread.unsettle", json!({"reason":"user"}), "Failed to un-settle thread")))
            } else {
                item("Settle thread".to_owned(), !can, Box::new(move || command("thread.settle", json!({}), "Failed to settle thread")))
            });
        }
        if view.caps.auto_settle_opt_out {
            // "Auto-settle behavior": a setting with the current choice marked, not a one-shot action.
            for (enabled, name) in [(true, "Auto-settle behavior: Enabled"), (false, "Auto-settle behavior: Disabled")] {
                let command = thread_command.clone();
                let checked = view.auto_settle == enabled;
                let disabled = !can || busy();
                entries.push(rsx! {button {class:"menu-item",role:"menuitemradio","aria-checked":"{checked}",r#type:"button",disabled,"aria-label":"{name}",
                    onclick:move |_| {close(); if !checked {command("thread.auto-settle.set", json!({"enabled":enabled}), "Failed to update auto-settle")}},"{name}"}});
            }
        }
        if view.caps.snooze {
            if view.snoozed.is_some() {
                let command = thread_command.clone();
                entries.push(item("Wake thread".to_owned(), !can, Box::new(move || command("thread.unsnooze", json!({"reason":"user"}), "Failed to wake thread"))));
            } else {
                let now = Utc::now();
                let offset = crate::scheduled_tasks_page::zone(now.timestamp_millis());
                for preset in sidebar::snooze_presets(now, offset) {
                    let command = thread_command.clone();
                    let local = preset.until.parse::<chrono::DateTime<Utc>>().map(|at| at + chrono::Duration::minutes(offset as i64)).ok();
                    let when = local.map(|at| { use chrono::Timelike; let hour = if at.hour() % 12 == 0 { 12 } else { at.hour() % 12 }; format!("{hour}:{:02} {}", at.minute(), if at.hour() < 12 { "AM" } else { "PM" }) }).unwrap_or_default();
                    let name = format!("Snooze: {} ({when})", preset.label);
                    let until = preset.until.clone();
                    entries.push(item(name, !can || !view.can_snooze, Box::new(move || command("thread.snooze", json!({"snoozedUntil":until}), "Failed to snooze thread"))));
                }
            }
        }
        if view.caps.title_regeneration {
            let command = thread_command.clone();
            entries.push(item((if view.regenerating { "Regenerating\u{2026}" } else { "Regenerate title" }).to_owned(), !can || view.regenerating, Box::new(move || command("thread.metadata.update", json!({"regenerateTitle":true}), "Failed to regenerate thread title"))));
        }
        entries.push(item("Rename thread".to_owned(), !can, Box::new(move || {let (mut settled, mut renaming) = (settled, renaming);settled.set(false);renaming.set(Some(title_now.clone()));})));
        if let Some(branch) = view.branch.clone() {
            // The branch carries over: the thread's worktree when it has one, else its branch on the checkout.
            let (transport, project, worktree) = (transport.clone(), view.project_id.clone(), view.worktree.clone());
            entries.push(item(format!("New thread on {branch}"), false, Box::new(move || {
                runtime::new_thread(&transport, state, Some(project.clone()));
                crate::new_thread::apply_prepared(state, &project, &branch, worktree.clone());
            })));
        }
        {
            let command = thread_command.clone();
            entries.push(item("Mark unread".to_owned(), !can || !view.has_completed_run, Box::new(move || command("thread.mark-unread", json!({}), "Could not mark the thread unread"))));
        }
        if let Some(path) = view.workspace.clone() {
            entries.push(item("Copy path".to_owned(), false, Box::new(move || copy_text(&path))));
        }
        if let Some(branch) = view.branch.clone() {
            entries.push(item("Copy branch".to_owned(), false, Box::new(move || copy_text(&branch))));
        }
        {
            let id = view.id.clone();
            entries.push(item("Copy thread ID".to_owned(), false, Box::new(move || copy_text(&id))));
        }
        {
            let (transport, id) = (transport.clone(), view.id.clone());
            entries.push(item("Archive thread".to_owned(), !can, Box::new(move || thread_lifecycle::request_archive(state, &transport, &id))));
        }
        {
            let (transport, id) = (transport.clone(), view.id.clone());
            entries.push(item("Delete thread".to_owned(), !can, Box::new(move || thread_lifecycle::request_delete(state, &transport, &id))));
        }
        entries
    } else {
        vec![]
    };
    // Enter or leaving the field commits, Escape cancels; an empty title is refused and an unchanged one is not sent.
    let commit = {
        let (transport, original) = (transport.clone(), view.title.clone());
        let id = id.clone();
        use_callback(move |typed: String| {
            if *settled.peek() {
                return;
            }
            settled.set(true);
            renaming.set(None);
            let typed = typed.trim().to_owned();
            if typed.is_empty() {
                notice::push(state, Level::Warning, "Thread title cannot be empty", "");
            } else if typed != original {
                send(&transport, state, busy, "thread.metadata.update", json!({"threadId":id,"title":typed}), "Could not rename the thread");
            }
        })
    };
    // A rename in progress when the connection stops allowing it is dropped.
    if renaming.peek().is_some() && !can {
        renaming.set(None);
    }
    let renamed = renaming.read().clone();
    let earlier = view.previous_providers.join(", ");
    rsx! {
        div {class:"thread-row-wrap","data-thread-id":"{view.id}","data-section":view.section.as_str(),"data-drops":"{view.drops}",
            if let Some(draft) = renamed {
                div {class:"{class} renaming","data-thread-status":"{status}",
                    span {class:"status-dot {status}","aria-hidden":"true"}
                    input {class:"thread-title-input","aria-label":"Thread title",value:"{draft}",
                        onmounted:move |event| {drop(event.set_focus(true));},
                        oninput:move |event| renaming.set(Some(event.value())),
                        onclick:move |event| event.stop_propagation(),
                        onkeydown:move |event| {
                            event.stop_propagation();
                            match event.key() {
                                Key::Enter => {event.prevent_default();let typed = renaming.peek().clone().unwrap_or_default();commit.call(typed);}
                                Key::Escape => {event.prevent_default();settled.set(true);renaming.set(None);}
                                _ => {}
                            }
                        },
                        onblur:move |_| {let typed = renaming.peek().clone().unwrap_or_default();commit.call(typed);},
                    }
                }
            } else {
            button {key:"{view.id}",class:"{class}","aria-label":"Thread {view.title}","data-thread-status":"{status}","aria-busy":"{busy()}","aria-current":if view.selected {"true"} else {"false"},
                onclick:{let transport=transport.clone();let id=id.clone();move |_| runtime::select_thread(&transport,state,id.clone())},
                oncontextmenu:move |event| {event.prevent_default();let at=event.client_coordinates();menu.set(Some((at.x,at.y)));},
                onkeydown:move |event| {if event.key()==Key::ContextMenu {event.prevent_default();menu.set(Some((0.0,0.0)));}},
                span {class:"status-dot {status}","aria-hidden":"true"}
                span {class:"thread-title","{view.title}"}
                if view.regenerating {span {class:"sr-only",role:"status","Regenerating title"}}
                if view.pinned {span {class:"thread-pin",title:"Pinned","aria-label":"Pinned","\u{25c6}"}}
                if let Some(wake)=&view.snoozed {span {class:"thread-snooze",title:"Snoozed",{format!("Snoozed \u{b7} {wake}")}}}
                else if matches!(view.status, Status::Limited | Status::Failed) {span {class:if view.status == Status::Limited {"thread-pill amber"} else {"thread-pill red"},role:"status",if view.status == Status::Limited {"Limited"} else {"Failed"}}}
                else if view.woke_at.is_some() {span {class:"thread-woke",title:"Woke",role:"status","\u{23f0} Woke"}}
                else if let Some(since)=view.working_since.clone() {WorkingFor {since, label: view.working_label}}
                else if let Some((label, tone))=label {span {class:"thread-pill {tone}",role:"status","{label}"}}
                if !view.time.is_empty() {span {class:"thread-time",title:"Last activity","{view.time}"}}
            }
            if view.branch.is_some() || view.pull_request.is_some() || !view.previous_providers.is_empty() {
                div {class:"thread-meta",
                    if let Some(branch)=view.branch.clone() {span {class:"thread-branch",title:"{branch}","{branch}"}}
                    if let Some(badge)=view.pull_request.clone() {
                        button {class:"thread-pr pr-{badge.state.class()}",r#type:"button","aria-label":"{badge.label}",title:"{badge.label}",
                            onclick:{
                                let transport=transport.clone();
                                move |_| {
                                    if let Some(target)=badge.target.clone() {
                                        runtime::select_thread(&transport,state,target.thread_id.clone());
                                        crate::pull_request_panel::open(state,target);
                                    }
                                }
                            },
                            crate::pull_request_glyph::Glyph {state:badge.state,stack:badge.stack}
                            span {class:"thread-pr-text","{badge.text}"}
                        }
                    }
                    if !view.previous_providers.is_empty() {span {class:"thread-providers",title:"Earlier: {earlier}","after {earlier}"}}
                }
            }
            if let Some(woke) = view.woke_at.clone().filter(|_| can) {
                // Dismissing is a visit at the moment of the wake, as in the original.
                button {class:"thread-woke-dismiss",r#type:"button","aria-label":"Dismiss Woke notification",title:"Dismiss Woke notification",disabled:busy(),
                    onclick:{let (transport, id) = (transport.clone(), id.clone());move |_| send(&transport,state,busy,"thread.visit",json!({"threadId":id,"visitedAt":woke}),"Could not dismiss the notification")},"\u{d7}"}
            }
            }
            if let Some((x,y))=menu() {
                div {class:"menu-backdrop",onclick:move |_| menu.set(None),oncontextmenu:move |event| {event.prevent_default();menu.set(None);}}
                div {class:"context-menu",role:"menu","aria-label":"Thread actions",style:if x > 0.0 || y > 0.0 {format!("position:fixed;left:{x}px;top:{y}px")} else {String::new()},tabindex:"0",
                    onmounted:move |event| {drop(event.set_focus(true));},
                    onkeydown:move |event| {if event.key()==Key::Escape {event.stop_propagation();menu.set(None);}},
                    {items.into_iter()}
                }
            }
        }
    }
}

/// The menu of a project's header: what the original's project context menu holds that this UI has
/// a handler for (the path to copy, and removing it with its own confirmation).
#[component]
pub(crate) fn ProjectMenu(state: Store<UiModel>, project_id: String, title: String, workspace_root: String, can_operate: bool) -> Element {
    let mut open = use_signal(|| false);
    let (id, path, remove_label) = (project_id.clone(), workspace_root.clone(), format!("Remove project {title}"));
    rsx! {
        div {class:"project-menu",
            button {class:"icon-button project-menu-button",r#type:"button","aria-label":"Project actions for {title}","aria-haspopup":"menu","aria-expanded":"{open()}",title:"Project actions",
                onclick:move |_| open.set(!open()),"\u{22ef}"}
            if open() {
                div {class:"menu-backdrop",onclick:move |_| open.set(false),oncontextmenu:move |event| {event.prevent_default();open.set(false);}}
                div {class:"context-menu project-menu-popup",role:"menu","aria-label":"Project actions",tabindex:"0",
                    onmounted:move |event| {drop(event.set_focus(true));},
                    onkeydown:move |event| {if event.key()==Key::Escape {event.stop_propagation();open.set(false);}},
                    button {class:"menu-item",role:"menuitem",r#type:"button","aria-label":"Copy project path",onclick:move |_| {open.set(false);copy_text(&path);},"Copy path"}
                    button {class:"menu-item",role:"menuitem",r#type:"button","aria-label":"{remove_label}",disabled:!can_operate,onclick:move |_| {open.set(false);thread_lifecycle::request_remove_project(state,&id);},"Remove project"}
                }
            }
        }
    }
}

/// What `thread.copyReference` copies for the open thread: its pull request's link when it has one,
/// else its ID (`resolveThreadReferenceCopyTarget`).
pub(crate) fn reference_target(state: Store<UiModel>) -> Option<(&'static str, String)> {
    let id = state.active_thread().read().clone()?;
    let field = state.shell();
    let shell = field.read();
    let thread = shell.snapshot.as_ref()?.threads.iter().find(|thread| thread.id.as_str() == id)?;
    let link = thread.linked_pull_request.clone().flatten().or_else(|| thread.branch_pull_request.clone().flatten()).map(|pull_request| pull_request.url.to_string());
    Some(match link {
        Some(url) => ("PR link", url),
        None => ("Thread ID", id),
    })
}
/// Copies it and says what was copied.
pub(crate) fn copy_reference(state: Store<UiModel>) {
    if let Some((what, value)) = reference_target(state) {
        copy_text(&value);
        notice::push(state, Level::Success, format!("{what} copied"), value);
    }
}
/// The ids of a thread's section among its project's threads in the order shown, and every
/// thread's order key (hidden ones too, so a new key never lands on a taken slot).
fn section_order(state: Store<UiModel>, thread_id: &str, section: sidebar::Section) -> (Vec<String>, std::collections::BTreeMap<String, Option<String>>) {
    let field = state.shell();
    let shell = field.read();
    let Some(snapshot) = shell.snapshot.as_ref() else { return Default::default() };
    let Some(project) = snapshot.threads.iter().find(|thread| thread.id.as_str() == thread_id).map(|thread| thread.project_id.clone()) else { return Default::default() };
    let sections = sidebar::Sections::of(listed(&snapshot.threads, project.as_str()), Utc::now(), caps_of(state).support());
    let pinned = section == sidebar::Section::Pinned;
    let keys = snapshot.threads.iter().map(|thread| (thread.id.to_string(), if pinned { thread.pin_order_key.clone() } else { thread.active_order_key.clone() }.flatten().map(|key| key.to_string()))).collect();
    (sections.section(section).iter().map(|thread| thread.id.to_string()).collect(), keys)
}

/// Pins or unpins the open thread (`thread.pin`).
pub(crate) fn toggle_pin(transport: &runtime::TransportHandle, state: Store<UiModel>) {
    let Some(id) = state.active_thread().peek().clone() else { return };
    let pinned = state.shell().peek().snapshot.as_ref().and_then(|snapshot| snapshot.threads.iter().find(|thread| thread.id.as_str() == id)).is_some_and(sidebar::is_pinned);
    runtime::command(transport, state, if pinned { "thread.unpin" } else { "thread.pin" }, json!({"threadId":id}));
}
/// Whether the open thread is pinned, for the palette's wording.
pub(crate) fn open_thread_pinned(state: Store<UiModel>) -> Option<bool> {
    let id = state.active_thread().read().clone()?;
    let field = state.shell();
    let shell = field.read();
    shell.snapshot.as_ref()?.threads.iter().find(|thread| thread.id.as_str() == id).map(sidebar::is_pinned)
}

/// Settles the open thread, or brings it back when it is settled (`thread.settle`).
pub(crate) fn toggle_settle(transport: &runtime::TransportHandle, state: Store<UiModel>) {
    let Some(id) = state.active_thread().peek().clone() else { return };
    let settled = open_thread_settled(state) == Some(true);
    runtime::command(transport, state, if settled { "thread.unsettle" } else { "thread.settle" }, if settled { json!({"threadId":id,"reason":"user"}) } else { json!({"threadId":id}) });
}
/// Whether the open thread is settled, for the palette's wording; none while the server cannot settle.
pub(crate) fn open_thread_settled(state: Store<UiModel>) -> Option<bool> {
    let caps = caps_of(state);
    if !caps.settlement {
        return None;
    }
    let id = state.active_thread().read().clone()?;
    let field = state.shell();
    let shell = field.read();
    let thread = shell.snapshot.as_ref()?.threads.iter().find(|thread| thread.id.as_str() == id)?;
    Some(sidebar::section_of(thread, Utc::now(), caps.support()) == sidebar::Section::Settled)
}
