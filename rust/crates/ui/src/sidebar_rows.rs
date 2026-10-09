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
use t3_client::sidebar::{self, Pill, Status, Tone};
use t3_contracts::ThreadShell;

/// What the server says it can do for threads. Each action is offered only where it is.
#[derive(Clone, Copy, PartialEq, Default)]
pub(crate) struct Caps {
    pub pinning: bool,
    pub snooze: bool,
    pub title_regeneration: bool,
}
pub(crate) fn caps(state: Store<UiModel>) -> Caps {
    let field = state.typed_config();
    let config = field.read();
    let capabilities = config.as_ref().map(|config| &config.environment.capabilities);
    Caps {
        pinning: capabilities.is_some_and(|c| c.thread_pinning == Some(true)),
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
    pub working_since: Option<String>,
    pub branch: Option<String>,
    pub workspace: Option<String>,
    /// The thread's own worktree, which "New thread on <branch>" reuses.
    pub worktree: Option<String>,
    pub has_completed_run: bool,
    pub can_snooze: bool,
    pub can_operate: bool,
    pub caps: Caps,
}

/// The rows of a project's threads in the order the sidebar lists them: pinned first, by their pin
/// order, then the rest as the server sent them, snoozed ones last.
pub(crate) fn rows<'a>(threads: &'a [ThreadShell], project_id: &t3_contracts::ProjectId, now: chrono::DateTime<Utc>) -> Vec<&'a ThreadShell> {
    let mut visible: Vec<&ThreadShell> = threads.iter().filter(|thread| &thread.project_id == project_id && thread.deleted_at.is_none() && thread.archived_at.is_none() && thread.lineage.relationship_to_parent != Some(t3_contracts::ThreadRelationship::Subagent)).collect();
    // Stable: ties keep the server's order.
    visible.sort_by_key(|thread| {
        if sidebar::is_pinned(thread) {
            (0, sidebar::pin_key(thread))
        } else {
            (if sidebar::snoozed_until(thread, now).is_some() { 2 } else { 1 }, (String::new(), 0))
        }
    });
    visible
}

pub(crate) fn view(thread: &ThreadShell, selected: bool, workspace: Option<String>, caps: Caps, can_operate: bool, now: chrono::DateTime<Utc>) -> RowView {
    let snoozed = sidebar::snoozed_until(thread, now);
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
        snoozed: snoozed.map(|wake| sidebar::wake_label(&wake.to_rfc3339(), now)),
        woke_at: sidebar::woke_at(thread, now).map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
        regenerating: matches!(thread.title_regeneration, Some(Some(_))),
        working_since: working.then(|| thread.activity_run_started_at.flatten().map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))).flatten(),
        branch: thread.branch.as_ref().map(ToString::to_string),
        workspace,
        worktree: thread.worktree_path.as_ref().map(ToString::to_string),
        has_completed_run: thread.latest_run_id.is_some() && !thread.status.is_active(),
        can_snooze: thread.pending_runtime_request.is_none(),
        can_operate,
        caps,
    }
}

fn tone_class(tone: Tone) -> &'static str {
    match tone {
        Tone::Amber => "amber",
        Tone::Indigo => "indigo",
        Tone::Sky => "sky",
        Tone::Muted => "muted",
        Tone::Violet => "violet",
        Tone::Emerald => "emerald",
    }
}

/// "Working for 3m" beside a working thread; the only part of a row that ticks.
#[component]
fn WorkingFor(since: String) -> Element {
    let mut now = use_signal(|| Utc::now().timestamp_millis());
    use_future(move || async move {
        loop {
            runtime::delay(1000).await;
            now.set(Utc::now().timestamp_millis());
        }
    });
    let started = chrono::DateTime::parse_from_rfc3339(&since).map(|at| at.timestamp_millis()).unwrap_or_else(|_| now());
    rsx! {span {class:"thread-duration","{sidebar::working_label(now() - started)}"}}
}

/// Puts text on the clipboard from the click that asked for it.
fn copy_text(text: &str) {
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
    let label = match (&view.pill, view.unread) {
        (Some(pill), _) => Some(pill.label),
        (None, true) => Some("Completed"),
        _ => None,
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
        // Where a pin sits among the project's pins: one key written to this thread, or the section's keys when a neighbour has none.
        if view.caps.pinning && view.pinned {
            let pinned = pinned_order(state, &view.id);
            for (up, name) in [(true, "Move pin up"), (false, "Move pin down")] {
                let next = t3_client::pin_order::moved(&pinned.0, &view.id, up);
                let (transport, id) = (transport.clone(), view.id.clone());
                let keys = pinned.1.clone();
                entries.push(item(name.to_owned(), !can || next.is_none(), Box::new(move || {
                    let Some(order) = next.clone() else { return };
                    for (target, key) in t3_client::pin_order::plan_reorder(&order, &keys, &id) {
                        runtime::command(&transport, state, "thread.pin.reorder", json!({"threadId":target,"orderKey":key}));
                    }
                })));
            }
        }
        if view.caps.pinning {
            let command = thread_command.clone();
            entries.push(if view.pinned { item("Unpin thread".to_owned(), !can, Box::new(move || command("thread.unpin", json!({}), "Could not unpin the thread"))) } else { item("Pin thread".to_owned(), !can, Box::new(move || command("thread.pin", json!({}), "Could not pin the thread"))) });
        }
        if view.caps.snooze {
            if view.snoozed.is_some() {
                let command = thread_command.clone();
                entries.push(item("Wake thread".to_owned(), !can, Box::new(move || command("thread.unsnooze", json!({"reason":"user"}), "Could not wake the thread"))));
            } else {
                let now = Utc::now();
                let offset = crate::scheduled_tasks_page::zone(now.timestamp_millis());
                for preset in sidebar::snooze_presets(now, offset) {
                    let command = thread_command.clone();
                    let local = preset.until.parse::<chrono::DateTime<Utc>>().map(|at| at + chrono::Duration::minutes(offset as i64)).ok();
                    let when = local.map(|at| { use chrono::Timelike; let hour = if at.hour() % 12 == 0 { 12 } else { at.hour() % 12 }; format!("{hour}:{:02} {}", at.minute(), if at.hour() < 12 { "AM" } else { "PM" }) }).unwrap_or_default();
                    let name = format!("Snooze: {} ({when})", preset.label);
                    let until = preset.until.clone();
                    entries.push(item(name, !can || !view.can_snooze, Box::new(move || command("thread.snooze", json!({"snoozedUntil":until}), "Could not snooze the thread"))));
                }
            }
        }
        if view.caps.title_regeneration {
            let command = thread_command.clone();
            entries.push(item((if view.regenerating { "Regenerating\u{2026}" } else { "Regenerate title" }).to_owned(), !can || view.regenerating, Box::new(move || command("thread.metadata.update", json!({"regenerateTitle":true}), "Could not regenerate the title"))));
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
    rsx! {
        div {class:"thread-row-wrap",
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
                else if view.woke_at.is_some() {span {class:"thread-woke",title:"Woke",role:"status","\u{23f0} Woke"}}
                else if let Some(since)=view.working_since.clone() {WorkingFor {since}}
                else if let Some(label)=label {span {class:if let Some(pill)=&view.pill {format!("thread-pill {}",tone_class(pill.tone))} else {"thread-pill emerald".to_owned()},"{label}"}}
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
/// The ids of the pinned threads of a thread's project in the order shown, with their order keys.
fn pinned_order(state: Store<UiModel>, thread_id: &str) -> (Vec<String>, std::collections::BTreeMap<String, Option<String>>) {
    let field = state.shell();
    let shell = field.read();
    let Some(snapshot) = shell.snapshot.as_ref() else { return Default::default() };
    let Some(project) = snapshot.threads.iter().find(|thread| thread.id.as_str() == thread_id).map(|thread| thread.project_id.clone()) else { return Default::default() };
    let pinned: Vec<&ThreadShell> = rows(&snapshot.threads, &project, Utc::now()).into_iter().filter(|thread| sidebar::is_pinned(thread)).collect();
    (pinned.iter().map(|thread| thread.id.to_string()).collect(), pinned.iter().map(|thread| (thread.id.to_string(), thread.pin_order_key.clone().flatten().map(|key| key.to_string()))).collect())
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
