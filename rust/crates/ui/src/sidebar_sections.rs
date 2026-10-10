//! A project's threads in the sections the sidebar lists them in (pinned, active, a Snoozed shelf
//! and a Settled shelf), the shelves that fold away, and moving a row by dragging it. Port of the
//! list of `Sidebar.tsx`; the rules (which thread is in which section, the order, what a drop
//! asks the server for) are `t3_client::sidebar`.
//!
//! The drag itself is the page's: `assets/sidebar_drag_abi.js` moves the lifted row with a
//! transform and marks where it would land, and reports one `drop` when the pointer is released,
//! so nothing here repaints while the pointer moves. A drop is held at its destination until the
//! server's state shows it, and given up (the row snaps back) when the server refuses.
use crate::{
    notice::{self, Level},
    runtime::{self, UiModel, UiModelStoreExt},
    settings_common::connected_allows,
    sidebar_rows::{self, Caps},
};
use chrono::Utc;
use dioxus::prelude::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use t3_client::sidebar::{self, DropPlan, Section, Sections};
use t3_contracts::{AuthEnvironmentScope, ThreadShell};

const SETTLED_KEY: &str = "t3code:sidebar:settled-expanded";
const SNOOZED_KEY: &str = "t3code:sidebar:snoozed-expanded";

async fn read_flag(key: &str) -> Option<bool> {
    let key = serde_json::to_string(key).expect("key");
    let stored = document::eval(&format!("try{{return window.localStorage.getItem({key});}}catch{{return null;}}")).join::<Option<String>>().await.ok().flatten()?;
    serde_json::from_str(&stored).ok()
}
fn write_flag(key: &str, value: bool) {
    let key = serde_json::to_string(key).expect("key");
    document::eval(&format!("try{{window.localStorage.setItem({key},'{value}');}}catch{{}}"));
}

/// Whether the Snoozed and Settled shelves are open. Both start closed and are remembered by the
/// browser, as the original's are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Shelves {
    pub snoozed: bool,
    pub settled: bool,
}
/// Reads what the browser remembered, once.
pub(crate) fn use_shelves(state: Store<UiModel>) {
    use_hook(move || {
        spawn(async move {
            let (snoozed, settled) = (read_flag(SNOOZED_KEY).await, read_flag(SETTLED_KEY).await);
            let mut shelves = state.shelves();
            let mut shelves = shelves.write();
            shelves.snoozed = snoozed.unwrap_or(shelves.snoozed);
            shelves.settled = settled.unwrap_or(shelves.settled);
        })
    });
}
fn toggle_shelf(state: Store<UiModel>, section: Section) {
    let mut shelves = state.shelves();
    let mut shelves = shelves.write();
    let (flag, key) = if section == Section::Snoozed { (&mut shelves.snoozed, SNOOZED_KEY) } else { (&mut shelves.settled, SETTLED_KEY) };
    *flag = !*flag;
    write_flag(key, *flag);
}

/// A drop waiting for the server: the row shows where it was dropped until the server's state
/// says so too, or the server refuses.
#[derive(Clone, PartialEq, Debug)]
pub(crate) struct Held {
    pub project: String,
    pub id: String,
    pub section: Section,
    /// The destination's order, with the row in place; empty for a settle.
    pub order: Vec<String>,
    /// The snapshot sequence when the server answered, once it has.
    pub answered_at: Option<u64>,
}
#[derive(Clone, PartialEq, Debug, Default)]
pub struct Reorder {
    pub held: Option<Held>,
    /// A drop is being written; the next waits.
    pub busy: bool,
}

/// The threads of a project in their sections, with a held drop applied.
pub(crate) fn sections_of<'a>(threads: &'a [ThreadShell], project_id: &'a str, now: chrono::DateTime<Utc>, caps: Caps, held: Option<&Held>) -> Sections<'a> {
    let mut sections = Sections::of(sidebar_rows::listed(threads, project_id), now, caps.support());
    let Some(held) = held.filter(|held| held.project == project_id) else { return sections };
    let Some(moved) = sidebar_rows::listed(threads, project_id).find(|thread| thread.id.as_str() == held.id) else { return sections };
    for section in [Section::Pinned, Section::Active, Section::Snoozed, Section::Settled] {
        sections.section_mut(section).retain(|thread| thread.id.as_str() != held.id);
    }
    let list = sections.section_mut(held.section);
    if held.section == Section::Settled {
        list.insert(0, moved);
    } else {
        list.push(moved);
        // The arranged order first; anything it does not name keeps its place after.
        list.sort_by_key(|thread| held.order.iter().position(|id| id == thread.id.as_str()).unwrap_or(usize::MAX));
    }
    sections
}

/// The rows a project lists. Reads only what its own rows are made of, so a drop in one project
/// does not rebuild another's.
#[component]
pub(crate) fn ProjectThreads(project_id: String, workspace_root: String, state: Store<UiModel>, transport: runtime::TransportHandle, caps: Caps, can_operate: bool, tick: u64) -> Element {
    let _ = tick;
    let mine = use_memo({
        let project_id = project_id.clone();
        move || state.reorder().read().held.clone().filter(|held| held.project == project_id)
    });
    let mut shown = use_signal(|| sidebar::SETTLED_INITIAL);
    let now = Utc::now();
    let open = state.active_thread().read().clone();
    let held = mine.read().clone();
    let field = state.shell();
    let shell = field.read();
    let Some(snapshot) = shell.snapshot.as_ref() else { return rsx! {} };
    let sections = sections_of(&snapshot.threads, &project_id, now, caps, held.as_ref());
    let row = |thread: &ThreadShell| {
        let workspace = thread.worktree_path.as_ref().map(ToString::to_string).or_else(|| Some(workspace_root.clone()));
        rsx! { sidebar_rows::ThreadRow { key: "{thread.id}", view: sidebar_rows::view(thread, open.as_deref() == Some(thread.id.as_str()), workspace, caps, can_operate, now), state, transport: transport.clone() } }
    };
    let (snoozed_open, settled_open) = (!sections.snoozed.is_empty() && state.shelves().read().snoozed, caps.settlement && state.shelves().read().settled);
    let snoozed_rows = sidebar::shelf_rows(&sections.snoozed, snoozed_open, usize::MAX, open.as_deref());
    let settled_rows = sidebar::shelf_rows(&sections.settled, settled_open, shown(), open.as_deref());
    let hidden = if settled_open { sections.settled.len().saturating_sub(settled_rows.len()) } else { 0 };
    let (snoozed_total, settled_total) = (sections.snoozed.len(), sections.settled.len());
    rsx! {
        div { class: "thread-section", "data-section": "pinned", "data-label": "Pinned", { sections.pinned.iter().map(|thread| row(thread)) } }
        div { class: "thread-section", "data-section": "active", "data-label": "Active", { sections.active.iter().map(|thread| row(thread)) } }
        if snoozed_total > 0 {
            div { class: "thread-shelf", "data-section": "snoozed",
                button { class: "shelf-header", r#type: "button", "aria-expanded": "{snoozed_open}", "aria-label": if snoozed_open { "Snoozed".to_owned() } else { format!("Snoozed ({snoozed_total})") }, onclick: move |_| toggle_shelf(state, Section::Snoozed),
                    if snoozed_open { "Snoozed" } else { "Snoozed ({snoozed_total})" } }
                { snoozed_rows.iter().map(|thread| row(thread)) }
            }
        }
        if caps.settlement {
            // Empty, it is only there for a drag to drop on.
            div { class: "thread-shelf", "data-section": "settled", "data-label": "Settled", "data-empty": "{settled_total == 0}",
                button { class: "shelf-header", r#type: "button", "aria-expanded": "{settled_open}", "aria-label": if settled_open { "Settled".to_owned() } else { format!("Settled ({settled_total})") }, onclick: move |_| toggle_shelf(state, Section::Settled),
                    if settled_open { "Settled" } else { "Settled ({settled_total})" } }
                { settled_rows.iter().map(|thread| row(thread)) }
                if hidden > 0 {
                    button { class: "shelf-more", r#type: "button", onclick: move |_| { let next = shown() + sidebar::SETTLED_PAGE; shown.set(next); }, "Show {hidden.min(sidebar::SETTLED_PAGE)} more" }
                }
            }
        }
    }
}

/// A drop as the page reports it: the row, the section it landed in and the row it landed before
/// (none: last).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DropRequest {
    pub id: String,
    pub section: Section,
    pub before: Option<String>,
}
impl DropRequest {
    fn from_json(message: &Value) -> Option<Self> {
        Some(Self { id: message["id"].as_str()?.to_owned(), section: Section::parse(message["section"].as_str()?)?, before: message["before"].as_str().map(str::to_owned) })
    }
}

struct Op {
    kind: &'static str,
    payload: Value,
    failure: &'static str,
}
/// The writes of a plan, in the order they are sent: the lifecycle change, then the order keys.
fn ops(plan: &DropPlan, id: &str) -> Vec<Op> {
    let thread = |kind, extra: Value, failure| {
        let mut payload = extra;
        payload["threadId"] = json!(id);
        Op { kind, payload, failure }
    };
    let order = |kind, failure, list: &[sidebar::Assignment]| list.iter().map(|a| Op { kind, payload: json!({"threadId":a.id,"orderKey":a.order_key}), failure }).collect::<Vec<_>>();
    match plan {
        DropPlan::None => vec![],
        DropPlan::Settle => vec![thread("thread.settle", json!({}), "Could not settle the thread")],
        DropPlan::ReorderPinned { assignments, .. } => order("thread.pin.reorder", "Could not reorder the pinned threads", assignments),
        DropPlan::Pin { order_key, extra, .. } => {
            let mut writes = vec![thread("thread.pin", order_key.as_ref().map_or(json!({}), |key| json!({"orderKey":key})), "Could not pin the thread")];
            writes.extend(order("thread.pin.reorder", "Could not reorder the pinned threads", extra));
            writes
        }
        DropPlan::MoveActive { assignments, unpin, unsettle, unsnooze, .. } => {
            let mut writes = vec![];
            if *unpin {
                writes.push(thread("thread.unpin", json!({}), "Could not unpin the thread"));
            }
            if *unsettle {
                writes.push(thread("thread.unsettle", json!({"reason":"user"}), "Could not unsettle the thread"));
            }
            if *unsnooze {
                writes.push(thread("thread.unsnooze", json!({"reason":"user"}), "Could not wake the thread"));
            }
            writes.extend(order("thread.active.reorder", "Could not reorder the threads", assignments));
            writes
        }
    }
}

/// What dropping `request` asks of the server, from the state as it is now.
fn plan_of(state: Store<UiModel>, caps: Caps, request: &DropRequest) -> Option<(String, DropPlan, Vec<String>)> {
    let field = state.shell();
    let shell = field.read();
    let snapshot = shell.snapshot.as_ref()?;
    let moved = snapshot.threads.iter().find(|thread| thread.id.as_str() == request.id)?;
    let project = moved.project_id.to_string();
    let now = Utc::now();
    let sections = Sections::of(sidebar_rows::listed(&snapshot.threads, &project), now, caps.support());
    let from = sidebar::section_of(moved, now, caps.support());
    let target = sidebar::drop_target(&sections, &request.id, request.section, request.before.as_deref())?;
    let ids = |list: &[&ThreadShell]| list.iter().map(|thread| thread.id.to_string()).collect::<Vec<_>>();
    let (pinned_order, active_order) = (ids(&sections.pinned), ids(&sections.active));
    let keys = |pinned: bool| -> BTreeMap<String, Option<String>> { snapshot.threads.iter().map(|thread| (thread.id.to_string(), if pinned { thread.pin_order_key.clone() } else { thread.active_order_key.clone() }.flatten().map(|key| key.to_string()))).collect() };
    // Every thread of this server stores an order, or none does.
    let all: BTreeSet<String> = snapshot.threads.iter().map(|thread| thread.id.to_string()).collect();
    let (pin_writable, active_writable) = (if caps.pinning && caps.pin_reorder { all.clone() } else { BTreeSet::new() }, if caps.active_reorder { all } else { BTreeSet::new() });
    let (pinned_keys, active_keys) = (keys(true), keys(false));
    let plan = sidebar::plan_drop(&sidebar::DropInput {
        moved: &request.id,
        from,
        moved_pinned: sidebar::is_pinned(moved),
        moved_settled: moved.settled_override == Some(t3_contracts::SettledOverride::Settled),
        supports_settlement: caps.settlement,
        target: &target,
        pinned_order: &pinned_order,
        pinned_keys: &pinned_keys,
        pin_writable: Some(&pin_writable),
        active_order: &active_order,
        active_keys: &active_keys,
        active_writable: Some(&active_writable),
    });
    let order = match request.section {
        Section::Pinned => target.pinned_order,
        Section::Active => target.active_order,
        _ => vec![],
    };
    Some((project, plan, order))
}

/// Whether the server's state has the held drop in place.
fn in_place(state: Store<UiModel>, caps: Caps, held: &Held) -> bool {
    let field = state.shell();
    let shell = field.read();
    let Some(snapshot) = shell.snapshot.as_ref() else { return false };
    let sections = Sections::of(sidebar_rows::listed(&snapshot.threads, &held.project), Utc::now(), caps.support());
    let current: Vec<String> = sections.section(held.section).iter().map(|thread| thread.id.to_string()).collect();
    if held.section == Section::Settled { current.contains(&held.id) } else { current == held.order }
}

/// One write of a drop. Not the banner: the drop says in its own words what failed.
async fn dispatch(transport: &runtime::TransportHandle, state: Store<UiModel>, kind: &str, mut payload: Value) -> Result<Value, String> {
    payload["type"] = json!(kind);
    payload["commandId"] = json!(uuid::Uuid::new_v4().to_string());
    runtime::request_value_reported(transport.clone(), state, "orchestration.dispatchCommand", payload).await
}

/// Writes a drop once. The row stays where it was dropped until the server's state shows it there;
/// a write the server refuses puts the row back and says so, and the writes after it are not sent.
pub(crate) fn drop_row(transport: &runtime::TransportHandle, state: Store<UiModel>, request: DropRequest) {
    {
        let reorder = state.reorder();
        let reorder = reorder.peek();
        if reorder.busy || reorder.held.is_some() || !connected_allows(state, AuthEnvironmentScope::OrchestrationOperate) {
            return;
        }
    }
    let caps = sidebar_rows::caps(state);
    let Some((project, plan, order)) = plan_of(state, caps, &request) else { return };
    let writes = ops(&plan, &request.id);
    if writes.is_empty() {
        return;
    }
    *state.reorder().write() = Reorder { held: Some(Held { project, id: request.id.clone(), section: request.section, order, answered_at: None }), busy: true };
    let transport = transport.clone();
    // Not tied to the row or menu that started it: a drop is written whatever is repainted meanwhile.
    dioxus::dioxus_core::spawn_forever(async move {
        let owner = runtime::response_owner(&transport, state);
        for write in writes {
            let result = dispatch(&transport, state, write.kind, write.payload).await;
            let now = runtime::response_owner(&transport, state);
            // Another server's state is not this drop's to hold.
            if owner.is_none() || !owner.as_ref().zip(now.as_ref()).is_some_and(|(before, after)| before.same_connection(after)) {
                *state.reorder().write() = Reorder::default();
                return;
            }
            if let Err(reason) = result {
                *state.reorder().write() = Reorder::default();
                notice::push(state, Level::Error, write.failure, reason);
                return;
            }
        }
        let sequence = state.shell().peek().snapshot.as_ref().map_or(0, |snapshot| snapshot.snapshot_sequence);
        let current = state.reorder().peek().held.clone();
        let mut reorder = state.reorder();
        match current {
            Some(current) if in_place(state, caps, &current) => *reorder.write() = Reorder::default(),
            Some(mut current) => {
                current.answered_at = Some(sequence);
                *reorder.write() = Reorder { held: Some(current), busy: false };
            }
            None => reorder.write().busy = false,
        }
    });
}

/// Listens for the page's drag and drops what it reports.
#[component]
pub(crate) fn DragBridge(state: Store<UiModel>, transport: runtime::TransportHandle) -> Element {
    let mut page = use_signal(|| None::<document::Eval>);
    use_future(move || {
        let transport = transport.clone();
        async move {
            let mut eval = document::eval(include_str!("../assets/sidebar_drag_abi.js"));
            page.set(Some(eval));
            while let Ok(message) = eval.recv::<Value>().await {
                if message["type"] == "drop" {
                    if let Some(request) = DropRequest::from_json(&message) {
                        drop_row(&transport, state, request);
                    }
                }
            }
        }
    });
    // A lift waits for the write in flight.
    use_effect(move || {
        let busy = state.reorder().read().busy;
        if let Some(eval) = *page.peek() {
            let _ = eval.send(json!({"type":"busy","busy":busy}));
        }
    });
    // The server's state replaces a held drop once it shows it, on the next change after its
    // answer, or when the connection that was asked is gone.
    use_effect(move || {
        let Some(current) = state.reorder().read().held.clone() else { return };
        let connected = *state.status().read() == t3_client::connection::ConnectionStatus::Connected;
        let sequence = state.shell().read().snapshot.as_ref().map_or(0, |snapshot| snapshot.snapshot_sequence);
        let settled = current.answered_at.is_some_and(|answered| answered != sequence) || (current.answered_at.is_some() && in_place(state, sidebar_rows::caps(state), &current));
        if !connected || settled {
            *state.reorder().write() = Reorder::default();
        }
    });
    use_drop(move || {
        if let Some(eval) = *page.peek() {
            let _ = eval.send(json!({"type":"stop"}));
        }
    });
    rsx! {}
}
