//! "Edit from here": rewinding a thread to before one of its messages, the original's
//! `RevertUserMessageButton`, "Edit from here?" dialog and `onRevertToTurnCount`. The message's
//! text and attachments return to the composer once the server has rewound the conversation.
//!
//! The server accepts a rewind at once and finishes it afterwards, so the work is a small
//! state machine in the model: a confirmation, then an accepted rewind that ends when the thread
//! projection shows the message's run rolled back, shows the server's failure for this request,
//! or the wait times out. Nothing here keeps a spinner: the status line is text, and every path
//! out of it clears it.
use crate::{
    attachments::{self, Incoming, Source},
    draft_storage,
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
};
use dioxus::prelude::*;
use serde_json::{Value, json};
use std::{cell::RefCell, rc::Rc};
use t3_client::{
    connection::ConnectionStatus,
    draft_storage::DraftTarget,
    rollback::{self, Progress, Target},
};

/// `(environment, thread)`: the thread a rewind belongs to.
pub type Owner = (String, String);

/// A message the reader asked to edit from, waiting for their choice.
#[derive(Debug, Clone, PartialEq)]
pub struct Confirm {
    pub owner: Owner,
    pub run_id: String,
    pub text: String,
    pub attachments: Vec<Value>,
    pub target: Target,
}

/// Attachment files fetched before the rewind starts, handed to the composer when it ends.
#[derive(Clone, Default)]
pub struct Prepared(Rc<RefCell<Vec<Incoming>>>);
impl std::fmt::Debug for Prepared {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "Prepared({} files)", self.0.borrow().len())
    }
}

/// A rewind the reader confirmed: fetching attachments, being accepted, or being carried out.
#[derive(Debug, Clone)]
pub struct Active {
    pub owner: Owner,
    pub command_id: String,
    pub run_id: String,
    pub restore_files: bool,
    /// The server has accepted the command; before that nothing can have changed.
    pub accepted: bool,
    pub text: String,
    pub files: Prepared,
}

#[derive(Debug, Clone, Default)]
pub struct Model {
    pub confirm: Option<Confirm>,
    pub active: Option<Active>,
    /// A refusal or a timeout, where the reader will see it.
    pub error: Option<(Owner, String)>,
    /// The request id of the server-recorded failure the reader dismissed.
    pub dismissed: Option<String>,
}

/// How long an accepted rewind may take before the wait is given up. Tests shorten it.
#[cfg(test)]
thread_local! {
    pub(crate) static TIMEOUT_OVERRIDE: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}
fn timeout_ms() -> u64 {
    #[cfg(test)]
    if let Some(milliseconds) = TIMEOUT_OVERRIDE.with(std::cell::Cell::get) {
        return milliseconds;
    }
    rollback::TIMEOUT_MS
}

fn fail(state: Store<UiModel>, owner: Owner, message: impl Into<String>) {
    state.rewind().write().error = Some((owner, message.into()));
}

/// Whether this thread has a rewind in flight, for sending and for the buttons.
pub(crate) fn is_rewinding(state: Store<UiModel>, owner: &Owner) -> bool {
    state.rewind().peek().active.as_ref().is_some_and(|active| &active.owner == owner)
}

/// Whether the thread has a run in progress, which a rewind would race.
fn thread_working(state: Store<UiModel>, thread_id: &str) -> bool {
    working(&state.shell().peek(), thread_id)
}
fn working(shell: &t3_client::shell::ShellState, thread_id: &str) -> bool {
    shell.snapshot.as_ref().is_some_and(|snapshot| snapshot.threads.iter().any(|thread| thread.id.as_str() == thread_id && thread.status.is_active()))
}

/// Whether the edit buttons are unavailable right now: shared by every button of the timeline
/// through one memo, so a turn starting or ending repaints only the buttons.
#[derive(Clone, Copy)]
struct Busy(Memo<bool>);

pub(crate) fn provide_busy(state: Store<UiModel>, owner: Owner) {
    let busy = use_memo(move || {
        let rewinding = state.rewind().read().active.as_ref().is_some_and(|active| active.owner == owner);
        rewinding || working(&state.shell().read(), &owner.1)
    });
    use_context_provider(|| Busy(busy));
}

/// The button on a user message whose turn can be rewound to before it.
#[component]
pub(crate) fn EditFromHere(
    state: Store<UiModel>,
    environment_key: String,
    thread_id: String,
    run_id: String,
    text: String,
    attachments: Vec<Value>,
    target: Target,
) -> Element {
    let busy = try_consume_context::<Busy>().map(|busy| busy.0);
    let disabled = busy.is_some_and(|busy| busy());
    let title = if disabled { "Wait for the current turn or rewind to finish" } else { "Edit from here" };
    rsx! {
        div { class: "message-actions",
            button { class: "icon-button edit-from-here", "aria-label": "Edit from here", title: "{title}", disabled,
                onclick: move |_| {
                    state.rewind().write().confirm = Some(Confirm {
                        owner: (environment_key.clone(), thread_id.clone()),
                        run_id: run_id.clone(),
                        text: text.clone(),
                        attachments: attachments.clone(),
                        target: target.clone(),
                    });
                },
                "\u{21b6}"
            }
        }
    }
}

/// Downloads the files of the message so they can return to the composer. Fetching first means
/// a message whose files are gone is not rewound at all, as in the original.
async fn prepare(transport: &TransportHandle, state: Store<UiModel>, environment_key: &str, list: &[Value]) -> Result<Vec<Incoming>, String> {
    let mut files = Vec::new();
    for attachment in list {
        if !matches!(attachment["type"].as_str(), Some("image" | "file")) {
            return Err("This message has an attachment that cannot be restored.".into());
        }
        let name = attachment["name"].as_str().unwrap_or("Attachment").to_owned();
        let unavailable = || format!("Could not restore attachment: {name}");
        let resource = json!({"_tag":"attachment","attachmentId":attachment["id"],"fileName":name,"mimeType":attachment["mimeType"]});
        let preview = attachments::asset_url(transport, state, environment_key, resource).await.map_err(|_| unavailable())?;
        // The signed URL is the whole credential: no cookies, no bearer token.
        let request = reqwest::Client::new().get(&preview.url);
        #[cfg(target_arch = "wasm32")]
        let request = request.fetch_credentials_omit();
        let response = request.send().await.map_err(|_| unavailable())?;
        if !response.status().is_success() {
            return Err(unavailable());
        }
        let bytes = response.bytes().await.map_err(|_| unavailable())?;
        files.push(Incoming { name, mime_type: attachment["mimeType"].as_str().unwrap_or("application/octet-stream").to_owned(), size: bytes.len() as u64, source: Source::Bytes(Rc::from(&bytes[..])) });
    }
    Ok(files)
}

/// Starts the rewind the reader confirmed.
fn start(transport: TransportHandle, state: Store<UiModel>, confirm: Confirm, restore_files: bool) {
    let owner = confirm.owner.clone();
    state.rewind().write().confirm = None;
    if thread_working(state, &owner.1) {
        return fail(state, owner, rollback::WORKING_MESSAGE);
    }
    let target = DraftTarget::thread(owner.0.clone(), owner.1.clone());
    {
        let field = state.draft_attachments();
        let drafts = field.peek();
        let held = drafts.get(&target).map(Vec::as_slice).unwrap_or(&[]);
        if held.iter().any(|attachment| matches!(attachment.upload, t3_client::attachments::Upload::Uploading | t3_client::attachments::Upload::Verifying)) {
            drop(drafts);
            return fail(state, owner, rollback::PREPARING_MESSAGE);
        }
        if held.len() + confirm.attachments.len() > t3_client::attachments::MAX_ATTACHMENTS {
            drop(drafts);
            return fail(state, owner, rollback::ROOM_MESSAGE);
        }
    }
    let command_id = uuid::Uuid::new_v4().to_string();
    let files = Prepared::default();
    {
        let mut field = state.rewind();
        let mut model = field.write();
        model.error = None;
        model.dismissed = None;
        model.active = Some(Active { owner: owner.clone(), command_id: command_id.clone(), run_id: confirm.run_id.clone(), restore_files, accepted: false, text: confirm.text.clone(), files: files.clone() });
    }
    // The rewind outlives the view that started it: leaving the thread must not strand it.
    dioxus::dioxus_core::spawn_forever(async move {
        let still_ours = |state: Store<UiModel>| state.rewind().peek().active.as_ref().is_some_and(|active| active.command_id == command_id);
        let fetched = prepare(&transport, state, &owner.0, &confirm.attachments).await;
        if !still_ours(state) {
            return;
        }
        match fetched {
            Ok(list) => *files.0.borrow_mut() = list,
            Err(message) => {
                let mut field = state.rewind();
                let mut model = field.write();
                model.active = None;
                model.error = Some((owner, message));
                return;
            }
        }
        let payload = json!({"type":"checkpoint.rollback","commandId":command_id,"threadId":owner.1,"scopeId":confirm.target.scope_id,"checkpointId":confirm.target.checkpoint_id,"restoreFiles":restore_files});
        let accepted = runtime::request_value_reported(transport.clone(), state, "orchestration.dispatchCommand", payload).await;
        if !still_ours(state) {
            return;
        }
        let mut field = state.rewind();
        let mut model = field.write();
        match accepted {
            Ok(_) => {
                if let Some(active) = model.active.as_mut() {
                    active.accepted = true;
                }
            }
            Err(message) => {
                model.active = None;
                model.error = Some((owner, message));
            }
        }
    });
}

/// The rewind finished: the message goes back to the composer, after what is already typed.
fn finish(transport: &TransportHandle, state: Store<UiModel>, owner: &Owner, command_id: &str) {
    let Some(active) = state.rewind().peek().active.clone().filter(|active| active.command_id == command_id) else {
        return;
    };
    state.rewind().write().active = None;
    // The composer shows the open thread; another one's draft is not ours to edit.
    if state.active_thread().peek().as_deref() != Some(owner.1.as_str()) || state.destination().peek().as_ref().map(ToString::to_string).as_deref() != Some(owner.0.as_str()) {
        return;
    }
    let current = state.draft().peek().clone();
    let next = rollback::restored_prompt(&current, &active.text);
    if next != current {
        draft_storage::edit_thread_prompt(state, next);
    }
    let files = std::mem::take(&mut *active.files.0.borrow_mut());
    if !files.is_empty() {
        attachments::attach(transport, state, &DraftTarget::thread(owner.0.clone(), owner.1.clone()), files);
    }
}

/// The status line, the failure of a rewind, and the confirmation, for the open thread.
#[component]
pub(crate) fn Host(state: Store<UiModel>, transport: TransportHandle, environment_key: String, thread_id: String) -> Element {
    let owner: Owner = (environment_key.clone(), thread_id.clone());
    let timer = use_hook(|| Rc::new(RefCell::new(None::<(String, dioxus::dioxus_core::Task)>)));

    // What the thread projection says about the accepted rewind. A memo, so streaming deltas
    // that change nothing about it repaint nothing.
    let progress = use_memo({
        let (owner, thread_id) = (owner.clone(), thread_id.clone());
        move || {
            let active = state.rewind().read().active.clone().filter(|active| active.owner == owner && active.accepted)?;
            let progress = rollback::progress(state.thread().read().projection.as_ref(), &thread_id, &active.command_id, &active.run_id);
            Some((active.command_id, progress))
        }
    });
    {
        let (owner, transport) = (owner.clone(), transport.clone());
        use_effect(move || match progress() {
            Some((command_id, Progress::Done)) => finish(&transport, state, &owner, &command_id),
            // The failure stays in the projection and is shown from there.
            Some((command_id, Progress::Failed(_))) => {
                if state.rewind().peek().active.as_ref().is_some_and(|active| active.command_id == command_id) {
                    state.rewind().write().active = None;
                }
            }
            _ => {}
        });
    }
    // The wait for an accepted rewind is bounded.
    {
        let (timer, owner) = (timer.clone(), owner.clone());
        use_effect(move || {
            let waiting = state.rewind().read().active.as_ref().filter(|active| active.owner == owner && active.accepted).map(|active| active.command_id.clone());
            let mut slot = timer.borrow_mut();
            if slot.as_ref().map(|(id, _)| id) == waiting.as_ref() {
                return;
            }
            if let Some((_, task)) = slot.take() {
                task.cancel();
            }
            if let Some(command_id) = waiting {
                let (owner, id) = (owner.clone(), command_id.clone());
                let task = spawn(async move {
                    runtime::delay(timeout_ms()).await;
                    let mut field = state.rewind();
                    let mut model = field.write();
                    if model.active.as_ref().is_some_and(|active| active.command_id == id) {
                        model.active = None;
                        model.error = Some((owner, rollback::TIMEOUT_MESSAGE.into()));
                    }
                });
                *slot = Some((command_id, task));
            }
        });
    }
    use_drop({
        let timer = timer.clone();
        move || {
            if let Some((_, task)) = timer.borrow_mut().take() {
                task.cancel();
            }
        }
    });

    let failure = use_memo({
        let thread_id = thread_id.clone();
        move || rollback::current_failure(state.thread().read().projection.as_ref(), &thread_id)
    });
    let field = state.rewind();
    let model = field.read();
    let active = model.active.clone().filter(|active| active.owner == owner);
    let error = model.error.clone().filter(|(error_owner, _)| *error_owner == owner).map(|(_, message)| message);
    let confirm = model.confirm.clone().filter(|confirm| confirm.owner == owner);
    let dismissed = model.dismissed.clone();
    drop(model);
    let recorded = failure().filter(|(request, _)| dismissed.as_deref() != Some(request.as_str()));
    let connected = *state.status().read() == ConnectionStatus::Connected;
    rsx! {
        if let Some(active) = active {
            div { class: "rewind-status", role: "status",
                if active.restore_files { "Rewinding the conversation and restoring files\u{2026}" } else { "Rewinding the conversation\u{2026}" }
            }
        } else if let Some(message) = error {
            div { class: "error-banner rewind-error", role: "alert", span { "{message}" }
                button { "aria-label": "Dismiss rewind error", onclick: move |_| state.rewind().write().error = None, "\u{d7}" } }
        } else if let Some((request, message)) = recorded {
            div { class: "error-banner rewind-error", role: "alert", span { "{message}" }
                button { "aria-label": "Dismiss rewind error", onclick: move |_| state.rewind().write().dismissed = Some(request.clone()), "\u{d7}" } }
        }
        if let Some(confirm) = confirm {
            div { class: "modal-backdrop",
                div { class: "dialog rewind-dialog", role: "alertdialog", "aria-modal": "true", "aria-labelledby": "rewind-heading", "aria-describedby": "rewind-description", tabindex: "-1",
                    onmounted: move |event| { let element = event.data(); spawn(async move { let _ = element.set_focus(true).await; }); },
                    onkeydown: move |event| { if event.key() == Key::Escape { state.rewind().write().confirm = None; } },
                    h2 { id: "rewind-heading", {rollback::CONFIRM_TITLE} }
                    p { id: "rewind-description", {rollback::CONFIRM_BODY} }
                    div { class: "dialog-actions",
                        button { "aria-label": "Cancel", r#type: "button", onclick: move |_| state.rewind().write().confirm = None, "Cancel" }
                        button { class: "destructive", "aria-label": rollback::REVERT_FILES, r#type: "button", disabled: !connected,
                            onclick: { let (transport, confirm) = (transport.clone(), confirm.clone()); move |_| start(transport.clone(), state, confirm.clone(), true) }, {rollback::REVERT_FILES} }
                        button { class: "primary", "aria-label": rollback::REVERT_KEEP, r#type: "button", disabled: !connected,
                            onclick: { let transport = transport.clone(); move |_| start(transport.clone(), state, confirm.clone(), false) }, {rollback::REVERT_KEEP} }
                    }
                }
            }
        }
    }
}
