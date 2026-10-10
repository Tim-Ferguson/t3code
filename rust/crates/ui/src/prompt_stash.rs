//! The prompt stash: put the prompt being written aside (with its attachments and chips) to carry
//! it elsewhere, and bring it back into whichever composer is open. Rules, wording and storage
//! shape are `t3_client::prompt_stash`. It is the browser's own and follows no thread: kept across
//! threads and reloads (`t3code:rust-prompt-stash:v1`), newest first, twenty at most. Restoring
//! adds to the open draft and leaves its model and mode alone, and takes the entry out; deleting it
//! deletes the uploads it held.
//!
//! The stash is its own field of the model, so stashing, restoring and the menu repaint the dock
//! above the composer and the composer's own fields, and nothing else.
use crate::{
    composer_chips,
    draft_storage,
    notice::{self, Level},
    runtime::{self, UiModel, UiModelStoreExt},
    settings_common::connected_allows,
};
use dioxus::prelude::*;
use t3_client::{
    attachments::{Persisted, Upload},
    draft_storage::{DraftKind, DraftTarget},
    prompt_stash::{self as logic, Entry, Stash},
};
use t3_contracts::AuthEnvironmentScope;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Model {
    pub entries: Vec<Entry>,
    pub menu: bool,
    /// A write is in flight: another stash waits for it.
    pub busy: bool,
    /// How many times a prompt was stashed, so the dock can say so to a screen reader.
    pub saved: u64,
}

// ---- storage ------------------------------------------------------------------------------

#[cfg(test)]
thread_local! {
    /// The stash's storage under test: nothing here touches a browser.
    pub(crate) static MEMORY: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
    pub(crate) static REJECT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
async fn read() -> Option<String> {
    #[cfg(test)]
    {
        return MEMORY.with(|memory| memory.borrow().clone());
    }
    #[cfg(all(not(test), target_arch = "wasm32"))]
    {
        return web_sys::window()?.local_storage().ok().flatten()?.get_item(logic::STORAGE_KEY).ok().flatten();
    }
    #[cfg(all(not(test), not(target_arch = "wasm32")))]
    {
        let key = serde_json::to_string(logic::STORAGE_KEY).expect("key");
        document::eval(&format!("try{{return window.localStorage.getItem({key});}}catch{{return null;}}")).join::<Option<String>>().await.ok().flatten()
    }
}
/// Writes the stash now, not after a pause: the composer is cleared on the strength of this landing.
async fn write(raw: &str) -> Result<(), String> {
    #[cfg(test)]
    {
        if REJECT.with(|reject| reject.get()) {
            return Err("Browser storage rejected the write.".into());
        }
        MEMORY.with(|memory| *memory.borrow_mut() = Some(raw.to_owned()));
        return Ok(());
    }
    #[cfg(all(not(test), target_arch = "wasm32"))]
    {
        return web_sys::window()
            .and_then(|window| window.local_storage().ok().flatten())
            .ok_or_else(|| "Browser storage is unavailable.".to_owned())?
            .set_item(logic::STORAGE_KEY, raw)
            .map_err(|_| "Browser storage rejected the write.".to_owned());
    }
    #[cfg(all(not(test), not(target_arch = "wasm32")))]
    {
        let (key, value) = (serde_json::to_string(logic::STORAGE_KEY).expect("key"), serde_json::to_string(raw).expect("value"));
        let stored = document::eval(&format!("try{{window.localStorage.setItem({key},{value});return true;}}catch{{return false;}}")).join::<bool>().await;
        if stored.unwrap_or(false) { Ok(()) } else { Err("Browser storage rejected the write.".into()) }
    }
}

/// Reads the stash once, when the app starts.
pub async fn hydrate(state: Store<UiModel>) {
    let raw = read().await;
    state.stash().write().entries = Stash::parse(raw.as_deref()).entries;
}

// ---- the draft being written ---------------------------------------------------------------

fn prompt_of(state: Store<UiModel>, target: &DraftTarget) -> String {
    let model = state.peek();
    match target.kind {
        DraftKind::Thread => model.draft.clone(),
        DraftKind::Project => target.environment.parse().ok().and_then(|environment| model.new_thread_drafts.get(&(environment, target.local_id.clone())).cloned()).unwrap_or_default(),
    }
}
fn set_prompt(state: Store<UiModel>, target: &DraftTarget, prompt: String) {
    match target.kind {
        DraftKind::Thread => {
            if state.peek().active_thread.as_deref() == Some(target.local_id.as_str()) {
                draft_storage::edit_thread_prompt(state, prompt);
            }
        }
        DraftKind::Project => {
            if let Ok(environment) = target.environment.parse() {
                draft_storage::edit_project_prompt(state, (environment, target.local_id.clone()), prompt);
            }
        }
    }
}
fn attachments_of(state: Store<UiModel>, target: &DraftTarget) -> Vec<t3_client::attachments::Attachment> {
    crate::attachments::load(state, target);
    state.draft_attachments().peek().get(target).cloned().unwrap_or_default()
}

// ---- stash, restore, delete -----------------------------------------------------------------

/// Puts the prompt, its attachments and its chips aside and empties the composer. The composer is
/// emptied only once the stash is written; a write the browser refuses leaves it as it was.
pub(crate) fn stash_current(transport: &runtime::TransportHandle, state: Store<UiModel>) {
    let Some(target) = composer_chips::open_target(state) else { return };
    if state.stash().peek().busy {
        return;
    }
    let prompt = prompt_of(state, &target).trim().to_owned();
    let attachments = attachments_of(state, &target);
    // An empty composer brings the stash back instead: the only entry at once, else the menu.
    if prompt.is_empty() && attachments.is_empty() {
        let entries = state.stash().peek().entries.clone();
        match entries.as_slice() {
            [only] => restore(transport, state, &only.id),
            _ => {
                let open = state.stash().peek().menu;
                state.stash().write().menu = !open;
            }
        }
        return;
    }
    let count = |matches: fn(&Upload) -> bool| attachments.iter().any(|attachment| matches(&attachment.upload));
    if let Some(reason) = logic::blocker(count(|upload| matches!(upload, Upload::Uploading | Upload::Verifying)), count(|upload| matches!(upload, Upload::NeedsReattach)), count(|upload| matches!(upload, Upload::Failed { .. }))) {
        notice::push(state, Level::Error, reason, "");
        return;
    }
    let chips = composer_chips::contexts_of(state, &target);
    let entry = Entry {
        id: uuid::Uuid::new_v4().to_string(),
        created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        environment: target.environment.clone(),
        prompt,
        attachments: attachments.iter().map(Persisted::of).collect(),
        records: chips.clone(),
    };
    let sent_attachments: Vec<String> = attachments.iter().map(|attachment| attachment.id.clone()).collect();
    let sent_chips: Vec<String> = chips.iter().filter_map(|chip| chip["contextId"].as_str().map(str::to_owned)).collect();
    let (next, evicted) = Stash { entries: state.stash().peek().entries.clone() }.with(entry);
    state.stash().write().busy = true;
    let transport = transport.clone();
    // Not tied to whatever started it: the write must finish whatever is unmounted meanwhile.
    dioxus::dioxus_core::spawn_forever(async move {
        let written = write(&next.to_json()).await;
        let mut model = state.stash();
        model.write().busy = false;
        if let Err(reason) = written {
            notice::push(state, Level::Error, "Could not stash this prompt", format!("{reason} The composer was left as it is."));
            return;
        }
        {
            let mut model = model.write();
            model.entries = next.entries;
            model.saved += 1;
        }
        // Everything the entry carries leaves the draft with it; the uploads stay on the server for the entry.
        set_prompt(state, &target, String::new());
        crate::attachments::sent(state, &target, &sent_attachments);
        composer_chips::sent(state, &target, &sent_chips);
        if let Some(evicted) = evicted {
            release(&transport, state, &evicted);
            notice::push(state, Level::Warning, "Oldest stashed prompt discarded", format!("The stash holds {} prompts; the oldest was removed to make room.", logic::MAX_ENTRIES));
        }
    });
}

/// The uploads an entry holds are no longer wanted.
fn release(transport: &runtime::TransportHandle, state: Store<UiModel>, entry: &Entry) {
    for attachment in &entry.attachments {
        if let Some(id) = &attachment.attachment_id {
            crate::attachments::delete_pending(transport, state, &entry.environment, id);
        }
    }
}

/// Takes an entry out of the stash and adds it to the open draft: its text after what is written,
/// its chips (each once), its attachments verified against the server as a reopened draft's are.
pub(crate) fn restore(transport: &runtime::TransportHandle, state: Store<UiModel>, id: &str) {
    let Some(target) = composer_chips::open_target(state) else { return };
    let current = state.stash().peek().entries.clone();
    let Some(entry) = current.iter().find(|entry| entry.id == id).cloned() else { return };
    if !entry.attachments.is_empty() && entry.environment != target.environment {
        notice::push(state, Level::Error, "Stashed files belong to another environment", "Restore this prompt in the environment that received its files.");
        return;
    }
    let (rest, _) = Stash { entries: current }.without(id);
    {
        let mut field = state.stash();
        let mut model = field.write();
        model.entries = rest.entries.clone();
        model.menu = false;
    }
    // The text, with the chips it names.
    let held = composer_chips::contexts_of(state, &target);
    let (restored_prompt, chips) = logic::bring_in(&entry.prompt, &entry.records, &held, &target.environment, || uuid::Uuid::new_v4().to_string());
    for chip in chips {
        composer_chips::add(state, target.clone(), chip);
    }
    let merged = logic::merge_prompt(&prompt_of(state, &target), &restored_prompt);
    if merged != prompt_of(state, &target) {
        set_prompt(state, &target, merged);
    }
    if !entry.attachments.is_empty() {
        let skipped = crate::attachments::bring_back(transport, state, &target, entry.attachments.clone());
        if !skipped.is_empty() {
            notice::push(state, Level::Warning, "Some attachments were not restored", format!("{} could not be restored: the composer is at its attachment limit.", skipped.join(", ")));
        }
    }
    persist_removal(state, rest);
}

/// Removes an entry and the uploads it held.
pub(crate) fn delete(transport: &runtime::TransportHandle, state: Store<UiModel>, id: &str) {
    let current = state.stash().peek().entries.clone();
    let (rest, taken) = Stash { entries: current }.without(id);
    let Some(taken) = taken else { return };
    {
        let mut field = state.stash();
        let mut model = field.write();
        model.entries = rest.entries.clone();
        if rest.entries.is_empty() {
            model.menu = false;
        }
    }
    release(transport, state, &taken);
    persist_removal(state, rest);
}
/// The removal is already shown; a write that fails says the entry could come back.
fn persist_removal(state: Store<UiModel>, rest: Stash) {
    dioxus::dioxus_core::spawn_forever(async move {
        if write(&rest.to_json()).await.is_err() {
            notice::push(state, Level::Warning, "Stash entry may come back", "Browser storage rejected the update, so this prompt could reappear after a reload.");
        }
    });
}

/// `composer.stash`: stash what is written, or bring the stash back when nothing is.
pub(crate) fn shortcut(transport: &runtime::TransportHandle, state: Store<UiModel>) {
    if !connected_allows(state, AuthEnvironmentScope::OrchestrationOperate) {
        return;
    }
    // An agent waiting for an answer: the stash can be looked at, but nothing is put into it.
    if let Some(id) = state.active_thread().peek().clone() {
        let field = state.shell();
        let shell = field.read();
        if let Some(thread) = shell.snapshot.as_ref().and_then(|snapshot| snapshot.threads.iter().find(|thread| thread.id.as_str() == id)) {
            if t3_client::sidebar::has_pending_input(thread) {
                drop(shell);
                let open = state.stash().peek().menu;
                state.stash().write().menu = !open;
                return;
            }
            if t3_client::sidebar::has_pending_approval(thread) {
                return;
            }
        }
    }
    stash_current(transport, state);
}

// ---- the dock ---------------------------------------------------------------------------------

/// The stash above the composer: a tab with the count, which opens the list. Drawn only when
/// there is something stashed, or the list was asked for.
#[component]
pub(crate) fn Dock(state: Store<UiModel>, transport: runtime::TransportHandle) -> Element {
    let model = state.stash().read().clone();
    let mut highlighted = use_signal(|| None::<String>);
    if model.entries.is_empty() && !model.menu {
        return rsx! {};
    }
    let count = model.entries.len();
    let active = highlighted.read().clone().filter(|id| model.entries.iter().any(|entry| entry.id == *id)).or_else(|| model.entries.first().map(|entry| entry.id.clone()));
    let hint = try_consume_context::<crate::keys::Keys>().and_then(|keys| keys.label("composer.stash"));
    let now = chrono::Utc::now();
    let ids: Vec<String> = model.entries.iter().map(|entry| entry.id.clone()).collect();
    rsx! {
        div { class: "stash-dock",
            span { class: "sr-only", role: "status", if model.saved > 0 { "Prompt stashed. {count} in the stash." } }
            if !model.menu {
                button { class: "stash-badge", r#type: "button", "aria-label": "Stashed prompts: {count}. Open stash.", "aria-expanded": "false", onmousedown: move |event| event.prevent_default(),
                    onclick: move |_| state.stash().write().menu = true,
                    "Stash " span { class: "stash-count", "{count}" } }
            } else {
                div { class: "stash-menu", role: "group", "aria-label": "Stash", tabindex: "0",
                    onmounted: move |event| { drop(event.set_focus(true)); },
                    onkeydown: {
                        let transport = transport.clone();
                        let (ids, active) = (ids.clone(), active.clone());
                        move |event: KeyboardEvent| {
                            let at = active.as_ref().and_then(|active| ids.iter().position(|id| id == active));
                            match event.key() {
                                Key::Escape => { event.prevent_default(); event.stop_propagation(); state.stash().write().menu = false; }
                                Key::ArrowDown | Key::ArrowUp if !ids.is_empty() => {
                                    event.prevent_default();
                                    event.stop_propagation();
                                    let step = if event.key() == Key::ArrowDown { 1 } else { ids.len() - 1 };
                                    highlighted.set(Some(ids[(at.map_or(ids.len() - 1, |at| at) + step) % ids.len()].clone()));
                                }
                                Key::Enter => {
                                    // A focused button inside the row owns its own activation.
                                    if let Some(id) = active.clone().filter(|_| !ids.is_empty()) { event.prevent_default(); event.stop_propagation(); restore(&transport, state, &id); }
                                }
                                Key::Backspace if event.modifiers().meta() || event.modifiers().ctrl() => {
                                    if let Some(id) = active.clone() { event.prevent_default(); event.stop_propagation(); delete(&transport, state, &id); }
                                }
                                _ => {}
                            }
                        }
                    },
                    button { class: "stash-header", r#type: "button", "aria-label": "Close stash", "aria-expanded": "true", onmousedown: move |event| event.prevent_default(), onclick: move |_| state.stash().write().menu = false,
                        "Stash " span { class: "stash-count", "{count}" } }
                    ul { role: "list", "aria-label": "Stashed prompts",
                        if model.entries.is_empty() {
                            li { class: "muted", "Nothing stashed yet."
                                if let Some(hint) = hint.clone() { " Press {hint} with a prompt in the composer to stash it." } }
                        }
                        for entry in model.entries.iter() {
                            li { key: "{entry.id}", "data-highlighted": "{active.as_deref() == Some(entry.id.as_str())}", class: if active.as_deref() == Some(entry.id.as_str()) { "selected" } else { "" },
                                onmousemove: { let id = entry.id.clone(); move |_| if highlighted.peek().as_deref() != Some(id.as_str()) { highlighted.set(Some(id.clone())) } },
                                button { class: "stash-restore", r#type: "button", "aria-label": "Restore stashed prompt: {logic::snippet(entry)}", onmousedown: move |event| event.prevent_default(),
                                    onclick: { let (transport, id) = (transport.clone(), entry.id.clone()); move |_| restore(&transport, state, &id) },
                                    "{logic::snippet(entry)}" }
                                if !entry.attachments.is_empty() { span { class: "muted", "{entry.attachments.len()} attached" } }
                                time { class: "muted", datetime: "{entry.created_at}", "{logic::relative_label(&entry.created_at, now)}" }
                                button { class: "stash-delete", r#type: "button", "aria-label": "Delete stashed prompt", onmousedown: move |event| event.prevent_default(),
                                    onclick: { let (transport, id) = (transport.clone(), entry.id.clone()); move |_| delete(&transport, state, &id) }, "\u{d7}" }
                            }
                        }
                    }
                }
            }
        }
    }
}
