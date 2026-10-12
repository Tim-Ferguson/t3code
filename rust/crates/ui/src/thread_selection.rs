//! Choosing several threads in the sidebar (Ctrl/Cmd+click toggles a row, Shift+click extends from
//! the anchor) and the menu of bulk actions on them. Port of the multi-select parts of `Sidebar.tsx`.
//! The chosen rows are model state, so a click repaints the rows whose choice changed and the
//! menu, not the conversation.
use crate::{
    keys,
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
    sidebar_rows, thread_lifecycle,
};
use chrono::Utc;
use dioxus::prelude::*;
use serde_json::json;
use t3_client::{
    sidebar,
    thread_selection::{self as logic, Action},
};
use t3_contracts::AuthEnvironmentScope;

/// Whether a click has the key that toggles a row (Cmd on a Mac, Ctrl elsewhere).
pub(crate) fn toggles(modifiers: Modifiers) -> bool {
    if keys::platform().is_mac() { modifiers.meta() } else { modifiers.ctrl() }
}

/// A click on a row: toggles, extends, or leaves the choice and opens the thread. True when the
/// click was a choice and the thread should not open. The choice is of the open environment's rows (the bulk actions are written
/// to its server), so a click on a row of another environment, with or without a key, opens that thread.
pub(crate) fn click(state: Store<UiModel>, id: &str, here: bool, modifiers: Modifiers) -> bool {
    if here && toggles(modifiers) {
        state.selection().write().toggle(id);
        return true;
    }
    if here && modifiers.shift() {
        let order: Vec<String> = keys::sidebar_order(state).into_iter().filter_map(|(environment, id)| crate::fleet::is_open(state, &environment).then_some(id)).collect();
        state.selection().write().range_to(id, &order);
        return true;
    }
    // Opening a thread ends a choice and makes it the anchor of the next range.
    let mut selection = state.selection();
    if selection.peek().has_selection() {
        selection.write().clear();
    }
    selection.write().set_anchor(id);
    false
}

fn operate(state: Store<UiModel>) -> bool {
    *state.status().peek() == t3_client::connection::ConnectionStatus::Connected && state.destination().peek().as_ref().is_some_and(|destination| state.environments().peek().allows(destination, AuthEnvironmentScope::OrchestrationOperate))
}

/// The chosen threads that still exist, in the order chosen, with what the menu needs to know of each.
fn chosen(state: Store<UiModel>) -> Vec<(t3_contracts::ThreadShell, logic::Facts)> {
    let selection = state.selection().peek().clone();
    let caps = sidebar_rows::caps(state);
    let now = Utc::now();
    let field = state.shell();
    let shell = field.read();
    let Some(snapshot) = shell.snapshot.as_ref() else { return vec![] };
    snapshot
        .threads
        .iter()
        .filter(|thread| selection.contains(thread.id.as_str()) && thread.deleted_at.is_none() && thread.archived_at.is_none())
        .map(|thread| {
            let facts = logic::Facts {
                id: thread.id.to_string(),
                settled: sidebar::section_of(thread, now, caps.support()) == sidebar::Section::Settled,
                pinned: sidebar::is_pinned(thread),
                can_snooze: thread.pending_runtime_request.is_none(),
                title_regeneration: caps.title_regeneration,
                regenerating: matches!(thread.title_regeneration, Some(Some(_))),
            };
            (thread.clone(), facts)
        })
        .collect()
}

/// The menu of the chosen rows (the original's `handleMultiSelectContextMenu`), shown where the pointer is and carried out when
/// something is chosen. Snooze opens the presets, and "Custom\u{2026}" the dialog.
async fn run_menu(state: Store<UiModel>, transport: TransportHandle, at: (f64, f64)) {
    use t3_client::context_menu::Item;
    let caps = sidebar_rows::caps(state);
    let rows = chosen(state);
    let facts: Vec<logic::Facts> = rows.iter().map(|(_, facts)| facts.clone()).collect();
    let can = operate(state);
    let offset = crate::scheduled_tasks_page::zone(Utc::now().timestamp_millis());
    let presets = sidebar::snooze_presets(Utc::now(), offset);
    let format = state.client_settings().peek().timestamp_format;
    let name = |action: Action| match action {
        Action::Unpin => "unpin",
        Action::Settle => "settle",
        Action::Snooze => "snooze",
        Action::RegenerateTitles => "regenerate-title",
        Action::MarkUnread => "mark-unread",
        Action::Delete => "delete",
    };
    let items: Vec<Item> = logic::menu(&facts, logic::Support { pinning: caps.pinning, settlement: caps.settlement, snooze: caps.snooze }, can)
        .into_iter()
        .map(|entry| {
            let mut item = Item::new(name(entry.action), entry.label).disabled(entry.disabled);
            item.destructive = entry.destructive;
            if entry.action == Action::Snooze {
                let mut children: Vec<Item> = presets
                    .iter()
                    .map(|preset| {
                        let local = preset.until.parse::<chrono::DateTime<Utc>>().map(|at| at + chrono::Duration::minutes(offset as i64)).ok();
                        let when = local.map(|at| sidebar::clock_label(at, format)).unwrap_or_default();
                        Item::new(format!("snooze:{}", preset.id), format!("{} ({when})", preset.label)).disabled(entry.disabled)
                    })
                    .collect();
                children.push(Item::new("snooze:custom", "Custom\u{2026}").separated().disabled(entry.disabled));
                item = item.children(children);
            }
            item
        })
        .collect();
    if items.is_empty() {
        return;
    }
    let Some(choice) = crate::context_menu::show("Selected threads", items, Some(at)).await else { return };
    let each = |kind: &str, ids: Vec<String>, extra: serde_json::Value| {
        for id in ids {
            let mut payload = extra.clone();
            payload["threadId"] = json!(id);
            runtime::command(&transport, state, kind, payload);
        }
    };
    // The rows are read again: what is chosen is what the person sees now.
    let rows = chosen(state);
    match choice.as_str() {
        "unpin" => each("thread.unpin", rows.iter().filter(|(_, facts)| facts.pinned).map(|(_, facts)| facts.id.clone()).collect(), json!({})),
        "settle" => each("thread.settle", rows.iter().filter(|(_, facts)| !facts.settled).map(|(_, facts)| facts.id.clone()).collect(), json!({})),
        "regenerate-title" => each("thread.metadata.update", rows.iter().filter(|(_, facts)| facts.title_regeneration && !facts.regenerating).map(|(_, facts)| facts.id.clone()).collect(), json!({"regenerateTitle":true})),
        "mark-unread" => each("thread.mark-unread", rows.iter().filter(|(thread, _)| thread.latest_run_id.is_some() && !thread.status.is_active()).map(|(_, facts)| facts.id.clone()).collect(), json!({})),
        "delete" => {
            thread_lifecycle::request_delete_many(state, &transport, rows.iter().map(|(_, facts)| facts.id.clone()).collect());
            return;
        }
        "snooze:custom" => {
            state.custom_snooze_thread().set(None);
            state.custom_snooze().set(true);
            return;
        }
        id => {
            let Some(preset) = presets.iter().find(|preset| format!("snooze:{}", preset.id) == id) else { return };
            each("thread.snooze", rows.iter().map(|(_, facts)| facts.id.clone()).collect(), json!({"snoozedUntil":preset.until}));
        }
    }
    state.selection().write().clear();
}

/// Right-click on a chosen row opens the bulk menu in place of the row's own.
pub(crate) fn open_menu(state: Store<UiModel>, transport: &TransportHandle, at: (f64, f64)) -> bool {
    if !state.selection().peek().has_selection() {
        return false;
    }
    if chosen(state).is_empty() {
        return false;
    }
    let transport = transport.clone();
    spawn(async move { run_menu(state, transport, at).await });
    true
}

/// Leaves out of the choice what is gone from the sidebar, so a menu never counts rows that cannot be seen.
pub(crate) fn prune(state: Store<UiModel>) {
    let selection = state.selection().peek().clone();
    if !selection.has_selection() {
        return;
    }
    let present: std::collections::BTreeSet<String> = state.shell().peek().snapshot.as_ref().map(|snapshot| snapshot.threads.iter().filter(|thread| thread.deleted_at.is_none() && thread.archived_at.is_none()).map(|thread| thread.id.to_string()).collect()).unwrap_or_default();
    let gone: Vec<String> = selection.keys.iter().filter(|key| !present.contains(*key)).cloned().collect();
    if !gone.is_empty() {
        state.selection().write().remove(&gone);
    }
}

/// The custom snooze dialog (`CustomSnoozeDialog.tsx`): a date and time, or a length of time, for the chosen threads.
#[component]
pub(crate) fn CustomSnoozeHost(state: Store<UiModel>, transport: TransportHandle) -> Element {
    if !*state.custom_snooze().read() {
        return rsx! {};
    }
    rsx! { CustomSnooze { state, transport } }
}

#[component]
fn CustomSnooze(state: Store<UiModel>, transport: TransportHandle) -> Element {
    use t3_client::custom_snooze as logic;
    let offset = crate::scheduled_tasks_page::zone(Utc::now().timestamp_millis());
    let initial = Utc::now() + chrono::Duration::hours(1) + chrono::Duration::minutes(i64::from(offset));
    let mut by_date = use_signal(|| true);
    let mut date = use_signal(|| initial.format("%Y-%m-%d").to_string());
    let mut time = use_signal(|| initial.format("%H:%M").to_string());
    let mut amount = use_signal(|| "2".to_owned());
    let mut unit = use_signal(|| "hours".to_owned());
    let mut error = use_signal(|| None::<&'static str>);
    let close = move || { state.custom_snooze().set(false); state.custom_snooze_thread().set(None); };
    let submit = use_callback(move |()| {
        let (date, time, amount, unit_text) = (date.peek().clone(), time.peek().clone(), amount.peek().clone(), unit.peek().clone());
        let input = if *by_date.peek() { logic::Input::Date { date: &date, time: &time } } else { logic::Input::Duration { amount: &amount, unit: logic::Unit::parse(&unit_text).unwrap_or(logic::Unit::Hours) } };
        let Some(until) = logic::resolve(&input, Utc::now(), offset) else {
            error.set(Some(if *by_date.peek() { logic::DATE_ERROR } else { logic::DURATION_ERROR }));
            return;
        };
        // A row's own menu names its thread; the bulk menu snoozes the chosen ones.
        let own = state.custom_snooze_thread().peek().clone();
        match own {
            Some(thread_id) => { runtime::command(&transport, state, "thread.snooze", json!({"threadId": thread_id, "snoozedUntil": until})); }
            None => {
                for (_, facts) in chosen(state).into_iter().filter(|(_, facts)| facts.can_snooze) {
                    runtime::command(&transport, state, "thread.snooze", json!({"threadId": facts.id, "snoozedUntil": until}));
                }
                state.selection().write().clear();
            }
        }
        close();
    });
    rsx! {
        div { class: "modal-backdrop",
            div { class: "dialog", role: "dialog", "aria-modal": "true", "aria-labelledby": "custom-snooze-heading", tabindex: "-1",
                onmounted: move |event| { let element = event.data(); spawn(async move { let _ = element.set_focus(true).await; }); },
                onkeydown: move |event| { if event.key() == Key::Escape { event.stop_propagation(); close(); } else if event.key() == Key::Enter { event.prevent_default(); submit.call(()); } },
                div { class: "dialog-form",
                    h2 { id: "custom-snooze-heading", "Custom snooze" }
                    p { class: "muted", "Choose when snoozed threads return to your inbox." }
                    div { class: "publish-cards", role: "radiogroup", "aria-label": "Schedule type",
                        button { class: if by_date() { "publish-card selected" } else { "publish-card" }, r#type: "button", role: "radio", "aria-checked": "{by_date()}", "aria-label": "Date and time", onclick: move |_| { by_date.set(true); error.set(None); }, "Date and time" }
                        button { class: if by_date() { "publish-card" } else { "publish-card selected" }, r#type: "button", role: "radio", "aria-checked": "{!by_date()}", "aria-label": "Duration", onclick: move |_| { by_date.set(false); error.set(None); }, "Duration" }
                    }
                    if by_date() {
                        label { r#for: "snooze-date", "Date" }
                        input { id: "snooze-date", r#type: "date", required: true, "aria-label": "Date", value: "{date}", oninput: move |event| { date.set(event.value()); error.set(None); } }
                        label { r#for: "snooze-time", "Time" }
                        input { id: "snooze-time", r#type: "time", required: true, "aria-label": "Time", value: "{time}", oninput: move |event| { time.set(event.value()); error.set(None); } }
                    } else {
                        label { r#for: "snooze-amount", "Snooze for" }
                        input { id: "snooze-amount", r#type: "number", min: "0", step: "any", required: true, "aria-label": "Snooze for", value: "{amount}", oninput: move |event| { amount.set(event.value()); error.set(None); } }
                        label { r#for: "snooze-unit", "Unit" }
                        select { id: "snooze-unit", "aria-label": "Unit", value: "{unit}", onchange: move |event| { unit.set(event.value()); error.set(None); },
                            option { value: "minutes", selected: unit() == "minutes", "Minutes" }
                            option { value: "hours", selected: unit() == "hours", "Hours" }
                            option { value: "days", selected: unit() == "days", "Days" }
                        }
                    }
                    if let Some(message) = error() { p { class: "publish-error", role: "alert", "{message}" } }
                    div { class: "dialog-actions",
                        button { r#type: "button", "aria-label": "Cancel", onclick: move |_| close(), "Cancel" }
                        button { class: "primary", r#type: "button", "aria-label": "Snooze", onclick: move |_| submit.call(()), "Snooze" }
                    }
                }
            }
        }
    }
}
