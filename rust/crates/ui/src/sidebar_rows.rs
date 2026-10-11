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
use std::rc::Rc;
use t3_client::context_menu as menu;
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
    /// `threadVisitedTracking`: the server keeps when a thread was last visited and can mark it unread.
    pub visited_tracking: bool,
    /// The beta Working shelf (a client setting): busy threads fold out of Active.
    pub working_shelf: bool,
}
impl Caps {
    pub fn support(self) -> sidebar::Support {
        sidebar::Support { settlement: self.settlement, snooze: self.snooze, working_shelf: self.working_shelf }
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
        visited_tracking: capabilities.is_some_and(|c| c.thread_visited_tracking == Some(true)),
        working_shelf: state.client_settings().read().sidebar_working_shelf_enabled,
    }
}

#[derive(Clone, PartialEq)]
pub(crate) struct RowView {
    pub id: String,
    pub project_id: String,
    pub title: String,
    pub selected: bool,
    /// Chosen with Ctrl/Cmd+click or Shift+click, for a bulk action.
    pub chosen: bool,
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
    /// The providers the thread ran on before this one: the name and the driver of each.
    pub previous_providers: Vec<(String, String)>,
    pub pull_request: Option<PullRequestBadge>,
    pub branch: Option<String>,
    pub workspace: Option<String>,
    /// The thread's own worktree, which "New thread on <branch>" reuses.
    pub worktree: Option<String>,
    pub has_completed_run: bool,
    /// Archiving is refused while the thread works (`archive_blocked`).
    pub archive_blocked: bool,
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
/// The threads of every project of a row: a group's members, listed as one.
pub(crate) fn listed_in<'a>(threads: &'a [ThreadShell], project_ids: &'a [String]) -> impl Iterator<Item = &'a ThreadShell> {
    threads.iter().filter(move |thread| project_ids.iter().any(|id| id == thread.project_id.as_str()) && thread.deleted_at.is_none() && thread.archived_at.is_none() && thread.lineage.relationship_to_parent != Some(t3_contracts::ThreadRelationship::Subagent))
}
/// A project's threads in the order the sidebar lists them, whatever is collapsed: pinned, active,
/// working (the beta shelf), snoozed, settled.
pub(crate) fn rows<'a>(threads: &'a [ThreadShell], project_id: &'a str, now: chrono::DateTime<Utc>, support: sidebar::Support) -> Vec<&'a ThreadShell> {
    let sections = sidebar::Sections::of(listed(threads, project_id), now, support);
    [sections.pinned, sections.active, sections.working, sections.snoozed, sections.settled].concat()
}

/// The name and driver the provider stack of a row is drawn with, by instance id.
pub(crate) fn provider_names(state: Store<UiModel>) -> std::collections::BTreeMap<String, (String, String)> {
    let field = state.typed_config();
    let config = field.read();
    config.as_ref().map(|config| config.providers.0.iter().map(|provider| (provider.instance_id.to_string(), (provider.display_name.clone().flatten().map_or_else(|| provider.instance_id.to_string(), |name| name.to_string()), provider.driver.to_string()))).collect()).unwrap_or_default()
}

pub(crate) fn view(thread: &ThreadShell, selected: bool, workspace: Option<String>, caps: Caps, can_operate: bool, now: chrono::DateTime<Utc>, providers: &std::collections::BTreeMap<String, (String, String)>) -> RowView {
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
        chosen: false,
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
        previous_providers: thread.provider_instance_history.iter().filter(|instance| **instance != if sidebar::runtime_status(thread).is_some() { thread.provider_instance_id.clone() } else { thread.model_selection.instance_id.clone() }).map(|instance| providers.get(instance.as_str()).cloned().unwrap_or_else(|| (instance.to_string(), String::new()))).collect(),
        pull_request: pull_request_badge(thread),
        working_since: working.then(|| thread.activity_run_started_at.flatten().map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))).flatten(),
        branch: thread.branch.as_ref().map(ToString::to_string),
        workspace,
        worktree: thread.worktree_path.as_ref().map(ToString::to_string),
        has_completed_run: thread.latest_run_id.is_some() && !thread.status.is_active(),
        archive_blocked: thread_lifecycle::archive_blocked(thread),
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

/// Whether the thread has a live runtime (its provider instance is the one shown), as `runtime_status` says.
pub(crate) fn runtime_instance(thread: &ThreadShell) -> bool {
    sidebar::runtime_status(thread).is_some()
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
    let class = format!("thread-row{}{}{}{}", if view.selected { " selected" } else { "" }, if view.chosen { " chosen" } else { "" }, if view.snoozed.is_some() { " snoozed" } else { "" }, if view.unread { " unread" } else { "" });
    let can = view.can_operate;
    // The menu is the shared model (`t3_client::context_menu::thread_actions`), drawn by the platform: the system's own on the desktop.
    let open_menu: Rc<dyn Fn(Option<(f64, f64)>)> = {
        let (transport, view) = (transport.clone(), view.clone());
        Rc::new(move |at| {
            let (transport, view) = (transport.clone(), view.clone());
            spawn(async move { run_menu(state, transport, view, busy, renaming, settled, at).await });
        })
    };
    let mut row = use_signal(|| None::<Rc<MountedData>>);
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
    let earlier = view.previous_providers.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>().join(", ");
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
                "aria-pressed":if view.chosen {"true"} else {"false"},
                onclick:{let transport=transport.clone();let id=id.clone();move |event| {
                    if crate::thread_selection::click(state,&id,event.modifiers()) {return;}
                    runtime::select_thread(&transport,state,id.clone())
                }},
                onmounted:move |event| row.set(Some(event.data())),
                oncontextmenu:{let id=id.clone();let open_menu=open_menu.clone();let transport=transport.clone();move |event| {
                    event.prevent_default();
                    let at=event.client_coordinates();
                    // On a chosen row the menu is for all the chosen rows.
                    if state.selection().peek().contains(&id) && crate::thread_selection::open_menu(state,&transport,(at.x,at.y)) {return;}
                    open_menu(Some((at.x,at.y)));
                }},
                // From the keyboard the menu opens under the row.
                onkeydown:{let open_menu=open_menu.clone();move |event| {
                    if event.key()==Key::ContextMenu {
                        event.prevent_default();
                        let open_menu=open_menu.clone();
                        let row=row.peek().clone();
                        spawn(async move {
                            let rect=match row {Some(row)=>row.get_client_rect().await.ok(),None=>None};
                            open_menu(rect.map(|rect| (rect.min_x(),rect.max_y())));
                        });
                    }
                }},
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
                                        // A stack or several links open the thread's Pull requests tab; one request opens itself.
                                        if badge.opens_list && crate::thread_pull_requests_panel::supported(state) {
                                            crate::thread_pull_requests_panel::open(state,&target.thread_id);
                                        } else {
                                            crate::pull_request_panel::open(state,target);
                                        }
                                    }
                                }
                            },
                            crate::pull_request_glyph::Glyph {state:badge.state,stack:badge.stack}
                            span {class:"thread-pr-text","{badge.text}"}
                        }
                    }
                    if !view.previous_providers.is_empty() {span {class:"thread-providers",title:"Earlier: {earlier}",for (name,driver) in view.previous_providers.iter().cloned() {crate::provider_glyph::Glyph {key:"{name}",driver,name}}}}
                }
            }
            if let Some(woke) = view.woke_at.clone().filter(|_| can && caps(state).visited_tracking) {
                // Dismissing is a visit at the moment of the wake, as in the original.
                button {class:"thread-woke-dismiss",r#type:"button","aria-label":"Dismiss Woke notification",title:"Dismiss Woke notification",disabled:busy(),
                    onclick:{let (transport, id) = (transport.clone(), id.clone());move |_| send(&transport,state,busy,"thread.visit",json!({"threadId":id,"visitedAt":woke}),"Could not dismiss the notification")},"\u{d7}"}
            }
            }
        }
    }
}

/// The menu of a thread row, shown where the pointer is and carried out when something is chosen. What the original's row menu
/// offers (`buildThreadActionMenuItems`), with the three things this app adds before them: referencing the thread in the draft
/// being written, and the keyboard path of the drag that moves it among its section.
async fn run_menu(state: Store<UiModel>, transport: runtime::TransportHandle, view: RowView, busy: Signal<bool>, mut renaming: Signal<Option<String>>, mut settled: Signal<bool>, at: Option<(f64, f64)>) {
    let can = view.can_operate;
    let now = Utc::now();
    let offset = crate::scheduled_tasks_page::zone(now.timestamp_millis());
    let format = state.client_settings().peek().timestamp_format;
    let presets = if view.caps.snooze && view.snoozed.is_none() { sidebar::snooze_presets(now, offset) } else { vec![] };
    let mut items = menu::thread_actions(&menu::ThreadActions {
        can_operate: can,
        branch: view.branch.clone(),
        project_filter: None,
        pinned: view.pinned,
        settled: view.section == sidebar::Section::Settled,
        auto_settle_enabled: view.auto_settle,
        snoozed: view.snoozed.is_some(),
        can_snooze_now: view.can_snooze,
        regenerating_title: view.regenerating,
        running: view.archive_blocked,
        settlement: view.caps.settlement,
        auto_settle_opt_out: view.caps.auto_settle_opt_out,
        snooze: view.caps.snooze,
        pinning: view.caps.pinning,
        title_regeneration: view.caps.title_regeneration,
        snooze_presets: presets
            .iter()
            .map(|preset| {
                let local = preset.until.parse::<chrono::DateTime<Utc>>().map(|at| at + chrono::Duration::minutes(offset as i64)).ok();
                (preset.id.to_owned(), preset.label.to_owned(), local.map(|at| sidebar::clock_label(at, format)).unwrap_or_default())
            })
            .collect(),
    });
    // While a write of this row is in flight its choices are off: a second one would be dropped by `send`, silently.
    if *busy.peek() {
        fn off(items: &mut [menu::Item]) {
            for item in items {
                if menu::thread_action_requires_operate(&item.id) {
                    item.disabled = true;
                }
                if let Some(children) = &mut item.children {
                    off(children);
                }
            }
        }
        off(&mut items);
    }
    // Marking a thread that has not finished a run unread has nothing to mark.
    if let Some(item) = items.iter_mut().find(|item| item.id == "mark-unread") {
        item.disabled |= !view.has_completed_run || !caps(state).visited_tracking;
    }
    // Another thread's history, attached to the draft being written so the agent can read it.
    let target = crate::composer_chips::open_target(state);
    let environment = state.destination().peek().as_ref().map(ToString::to_string).unwrap_or_default();
    let own = target.as_ref().is_some_and(|target| target.local_id == view.id);
    let mut extras = vec![menu::Item::new("reference-in-chat", "Reference in chat").disabled(target.is_none() || own)];
    // Where a thread sits among its section: one key written to it, or the section's keys when a neighbour has none.
    // The keyboard path of the drag; only the pinned and the active sections are arranged.
    let mut moves: Vec<(&'static str, Option<Vec<String>>, std::collections::BTreeMap<String, Option<String>>)> = vec![];
    for (section, writable, up_name, down_name, kind) in [(sidebar::Section::Pinned, view.caps.pinning && view.caps.pin_reorder, "Move pin up", "Move pin down", "thread.pin.reorder"), (sidebar::Section::Active, view.caps.active_reorder, "Move thread up", "Move thread down", "thread.active.reorder")] {
        if view.section != section || !writable {
            continue;
        }
        let (order, keys) = section_order(state, &view.id, section);
        for (up, name) in [(true, up_name), (false, down_name)] {
            let next = t3_client::pin_order::moved(&order, &view.id, up);
            extras.push(menu::Item::new(format!("move:{}", moves.len()), name).disabled(!can || next.is_none()));
            moves.push((kind, next, keys.clone()));
        }
    }
    items.splice(0..0, extras);
    let Some(choice) = crate::context_menu::show("Thread actions", items, at).await else { return };
    let command = |kind: &'static str, extra: Value, failure: &'static str| {
        let mut payload = extra;
        payload["threadId"] = json!(view.id);
        send(&transport, state, busy, kind, payload, failure);
    };
    match choice.as_str() {
        "reference-in-chat" => {
            if let Some(target) = target {
                crate::composer_chips::add(state, target, t3_client::composer_context::thread_record(&environment, &view.id, &view.title));
            }
        }
        "new-thread-on-branch" => {
            // The branch carries over: the thread's worktree when it has one, else its branch on the checkout.
            if let Some(branch) = view.branch.clone() {
                runtime::new_thread(&transport, state, Some(view.project_id.clone()));
                crate::new_thread::apply_prepared(state, &branch, view.worktree.clone());
            }
        }
        "pin" => command("thread.pin", json!({}), "Failed to pin thread"),
        "unpin" => thread_lifecycle::request_unpin(state, &transport, &view.id),
        "settle" => command("thread.settle", json!({}), "Failed to settle thread"),
        "unsettle" => command("thread.unsettle", json!({"reason":"user"}), "Failed to un-settle thread"),
        "unsnooze" => command("thread.unsnooze", json!({"reason":"user"}), "Failed to wake thread"),
        "snooze:custom" => {
            state.custom_snooze_thread().set(Some(view.id.clone()));
            state.custom_snooze().set(true);
        }
        id if id.starts_with("snooze:") => {
            if let Some(preset) = presets.iter().find(|preset| format!("snooze:{}", preset.id) == id) {
                command("thread.snooze", json!({"snoozedUntil":preset.until}), "Failed to snooze thread");
            }
        }
        id @ ("auto-settle:enabled" | "auto-settle:disabled") => {
            let enabled = id == "auto-settle:enabled";
            if enabled != view.auto_settle {
                command("thread.auto-settle.set", json!({"enabled":enabled}), "Failed to update auto-settle");
            }
        }
        "rename" => {
            settled.set(false);
            renaming.set(Some(view.title.clone()));
        }
        "regenerate-title" => command("thread.metadata.update", json!({"regenerateTitle":true}), "Failed to regenerate thread title"),
        "mark-unread" => command("thread.mark-unread", json!({}), "Could not mark the thread unread"),
        "copy-path" => match view.workspace.clone() {
            Some(path) => copy_text(&path),
            None => notice::push(state, Level::Error, "Path unavailable", "This thread does not have a workspace path to copy."),
        },
        "copy-branch" => {
            if let Some(branch) = &view.branch {
                copy_text(branch);
            }
        }
        "copy-thread-id" => copy_text(&view.id),
        "project-settings" => crate::projects_page::open(state, &view.project_id),
        "archive" => thread_lifecycle::request_archive(state, &transport, &view.id),
        "delete" => thread_lifecycle::request_delete(state, &transport, &view.id),
        id => {
            if let Some((kind, Some(order), keys)) = id.strip_prefix("move:").and_then(|index| index.parse::<usize>().ok()).and_then(|index| moves.get(index).cloned()) {
                for (target, key) in t3_client::pin_order::plan_reorder(&order, &keys, &view.id) {
                    runtime::command(&transport, state, kind, json!({"threadId":target,"orderKey":key}));
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
    let scope = crate::sidebar_groups::scope_of(state, project.as_str());
    let sections = sidebar::Sections::of(listed_in(&snapshot.threads, &scope.ids), Utc::now(), caps_of(state).support());
    let pinned = section == sidebar::Section::Pinned;
    let keys = snapshot.threads.iter().map(|thread| (thread.id.to_string(), if pinned { thread.pin_order_key.clone() } else { thread.active_order_key.clone() }.flatten().map(|key| key.to_string()))).collect();
    (sections.section(section).iter().map(|thread| thread.id.to_string()).collect(), keys)
}

/// Pins or unpins the open thread (`thread.pin`).
pub(crate) fn toggle_pin(transport: &runtime::TransportHandle, state: Store<UiModel>) {
    let Some(id) = state.active_thread().peek().clone() else { return };
    let pinned = state.shell().peek().snapshot.as_ref().and_then(|snapshot| snapshot.threads.iter().find(|thread| thread.id.as_str() == id)).is_some_and(sidebar::is_pinned);
    if pinned {
        thread_lifecycle::request_unpin(state, transport, &id);
    } else {
        runtime::command(transport, state, "thread.pin", json!({"threadId":id}));
    }
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
