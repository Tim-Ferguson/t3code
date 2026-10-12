//! One message to several models: from a new thread's form the person picks several models (Shift-click in the picker) and
//! one thread per model starts, each in a worktree of its own made from the base branch. Port of `multipleModelSelections` in
//! `ChatView.tsx` (the send path in `onSend`, the draft's fan-out state `draftFanoutStateAtom`) and `ChatComposer.tsx` (the
//! picker's toggle). The rules that need no transport are `t3_client::multi_model`; the form's own part is in `new_thread.rs`.
//!
//! The original starts each thread with `startThreadTurn` and a worktree bootstrap; this app's one way to start a thread is
//! `orchestration.launchThread` with a `worktree` workspace strategy, which the server prepares the same way. Sending clears the
//! draft at once, as the original does, and a model that failed puts the prompt, the attachments and the chips back with that
//! model still chosen. A request whose answer was lost may have started its thread, so that model is not sent again until the
//! person has looked at the thread and allowed it.
//!
//! TODO(original start-from-origin): the original's `startFromOrigin` flag (a worktree made from the remote branch) has no control in this app's form,
//! so none is sent.
use crate::{
    notice,
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
};
use dioxus::prelude::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use t3_client::{
    attachments::Attachment,
    draft_storage::DraftTarget,
    multi_model::{self as logic, Refusal, Toggled},
    new_thread::InstanceModelOption,
};
use t3_contracts::{EnvironmentId, ModelSelection, ProviderInstanceId, RuntimeMode, ThreadLaunchResult};

pub(crate) type DraftKey = (EnvironmentId, String);

/// What a message that failed took out of its draft, until it is back.
#[derive(Debug, Clone)]
struct Taken {
    prompt: String,
    attachments: Vec<Attachment>,
    chips: Vec<Value>,
    failed: Vec<ModelSelection>,
}

/// The state the fan-out keeps per draft, in memory only (`draftFanoutStateAtom`): the models chosen together, which drafts have
/// requests going out, the threads a lost request may have started, and what a failed message took out of its draft.
#[derive(Debug, Clone, Default)]
pub struct Model {
    chosen: BTreeMap<DraftKey, Vec<ModelSelection>>,
    sending: std::collections::BTreeSet<DraftKey>,
    /// Retry key -> the thread its request may have started.
    uncertain: BTreeMap<String, String>,
    /// "Allow retry" asked: (notice, retry key, thread).
    asking: Option<(u64, String, String)>,
    lost: BTreeMap<u64, (DraftKey, Taken)>,
    next_token: u64,
}

/// The models chosen together for the draft, when more than one (or when a failed send left them).
pub(crate) fn chosen(state: Store<UiModel>, key: &DraftKey) -> Option<Vec<ModelSelection>> {
    state.multi_model().read().chosen.get(key).cloned()
}

fn set_chosen(state: Store<UiModel>, key: &DraftKey, selections: Option<Vec<ModelSelection>>) {
    let mut model = state.multi_model();
    let mut model = model.write();
    match selections {
        Some(selections) => {
            model.chosen.insert(key.clone(), selections);
        }
        None => {
            model.chosen.remove(key);
        }
    }
}

/// Whether the draft has its requests going out: it takes no second send meanwhile (`sendInFlight`).
pub(crate) fn sending(state: Store<UiModel>, key: &DraftKey) -> bool {
    state.multi_model().read().sending.contains(key)
}

/// A single choice again: the picker chose one model alone.
pub(crate) fn clear(state: Store<UiModel>, key: &DraftKey) {
    set_chosen(state, key, None);
}

/// The picker's Shift-click or Shift+Enter on a row (`onToggleModel`). A single choice again hands the model back to `single`,
/// which makes it the form's choice.
pub(crate) fn toggle(state: Store<UiModel>, key: &DraftKey, rows: &[InstanceModelOption], primary: &ModelSelection, instance: &ProviderInstanceId, model: &str, single: impl FnOnce(ModelSelection)) {
    let current = chosen(state, key);
    match logic::toggle(rows, current.as_deref(), primary, instance, model) {
        Toggled::Several(next) => set_chosen(state, key, Some(next)),
        Toggled::Single(only) => {
            set_chosen(state, key, None);
            single(only);
        }
    }
}

/// What the form hands over to start the threads.
pub(crate) struct Plan {
    pub key: DraftKey,
    pub project_id: String,
    /// The models chosen together, and the one the form shows as chosen (its options ride on it).
    pub selections: Vec<ModelSelection>,
    pub primary: ModelSelection,
    pub mode: RuntimeMode,
    pub base_ref: String,
    pub prompt: String,
    /// The project's folder, where Git is asked whether it is a repository.
    pub workspace_root: String,
}

/// `onSend` for several models. Nothing is sent, and nothing taken from the draft, until every check has passed; then the draft
/// is cleared and one thread is requested for each model. The answers are told by notices, and a model that failed puts its
/// message back.
pub(crate) fn start(transport: &TransportHandle, state: Store<UiModel>, plan: Plan) {
    let Some(config) = state.peek().typed_config.clone() else { return };
    // Nothing is checked or sent for a draft whose requests are still going out.
    if sending(state, &plan.key) {
        return;
    }
    match logic::refusal(logic::supported(&config), true, &plan.base_ref, plan.selections.len()) {
        Some(Refusal::UpdateServer) => {
            runtime::fail(state, logic::UPDATE_SERVER);
            return;
        }
        Some(Refusal::ChooseModelsAndBranch) => {
            notice::push(state, notice::Level::Warning, logic::CHOOSE_TITLE, logic::CHOOSE_DETAIL);
            return;
        }
        None => {}
    }
    state.multi_model().write().sending.insert(plan.key.clone());
    let transport = transport.clone();
    dioxus::dioxus_core::spawn_forever(async move {
        let flight = Flight { state, key: plan.key.clone(), released: false };
        send(&transport, state, plan, flight).await;
    });
}

/// The draft's claim on "requests going out": let go by `release` once they are out, and by a send that stops before them.
struct Flight {
    state: Store<UiModel>,
    key: DraftKey,
    released: bool,
}
impl Flight {
    fn release(&mut self) {
        if !self.released {
            self.released = true;
            self.state.multi_model().write().sending.remove(&self.key);
        }
    }
}
impl Drop for Flight {
    fn drop(&mut self) {
        self.release();
    }
}

/// Whether the project's folder is a Git repository. The original assumes it is until Git has said otherwise; so does this, when
/// Git cannot be asked.
async fn is_repository(transport: &TransportHandle, state: Store<UiModel>, folder: &str) -> bool {
    let page = runtime::request_value_reported(transport.clone(), state, t3_client::vcs::LIST_REFS, json!({"cwd":folder,"refKind":"local","limit":1})).await;
    page.ok().and_then(|page| page["isRepo"].as_bool()).unwrap_or(true)
}

/// One request: the thread for one model, its answer, and the thread id it was given.
struct Outcome {
    selection: ModelSelection,
    thread_id: String,
    retry_key: String,
    result: Result<(), String>,
}

async fn send(transport: &TransportHandle, state: Store<UiModel>, plan: Plan, mut flight: Flight) {
    let Plan { key, project_id, selections, primary, mode, base_ref, prompt, workspace_root } = plan;
    if !is_repository(transport, state, &workspace_root).await {
        notice::push(state, notice::Level::Warning, logic::CHOOSE_TITLE, logic::CHOOSE_DETAIL);
        return;
    }
    let Some(config) = state.peek().typed_config.clone() else { return };
    let client = state.peek().client_settings.clone();
    // Every model must be ready before any thread starts (`multipleTargets`). The options of the form ride on its own model. The
    // prompt goes as typed to each: the original's per-model effort prefix is what this form's effort control already writes
    // into the prompt.
    let mut targets: Vec<(ModelSelection, RuntimeMode)> = vec![];
    for selection in logic::with_primary(&selections, &primary) {
        if let Some(problem) = logic::provider_problem(&config, &selection) {
            runtime::fail(state, problem);
            return;
        }
        let Some(target) = t3_client::new_thread::launch_selection(&config, &client, &selection, mode) else {
            runtime::fail(state, format!("Provider for {} is unavailable.", selection.model));
            return;
        };
        targets.push(target);
    }
    if let Some(message) = t3_client::composer_submission::prompt_length_message(&prompt) {
        state.prompt_length_error().set(Some(message));
        return;
    }
    let draft = DraftTarget::draft(key.0.to_string(), key.1.clone());
    if crate::draft_storage::has_unrendered(state, &draft) {
        runtime::fail(state, "This saved draft contains context that the Rust composer cannot send yet. The original draft was preserved; open it in the original app to send all content.");
        return;
    }
    let (attachment_ids, attachments) = match crate::attachments::for_send(state, &draft) {
        Ok(sendable) => sendable,
        Err(error) => {
            runtime::fail(state, error);
            return;
        }
    };
    let (text, context, chip_ids) = crate::composer_chips::for_send(state, &draft, &prompt);
    if text.trim().is_empty() && attachments.is_empty() && context.is_none() {
        return;
    }
    let title = {
        let (image, file) = t3_client::thread_title::first_attachment_names(&json!(attachments));
        t3_client::thread_title::title_seed(&prompt, image.as_deref(), file.as_deref())
    };

    // The draft is cleared at once; what a failed model needs is kept to put back.
    let taken = Taken { prompt: prompt.clone(), attachments: crate::attachments::of_draft(state, &draft), chips: crate::composer_chips::contexts_of(state, &draft), failed: vec![] };
    crate::draft_storage::edit_draft_prompt(state, key.clone(), String::new());
    crate::attachments::sent(state, &draft, &attachment_ids);
    crate::composer_chips::sent(state, &draft, &chip_ids);
    let submitted = chosen(state, &key);
    let environment = key.0.clone();

    let mut requests = vec![];
    for (selection, mode) in targets {
        let retry_key = logic::retry_key(key.0.as_str(), &key.1, &selection);
        let retained = state.multi_model().read().uncertain.get(&retry_key).cloned();
        let thread_id = retained.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let transport = transport.clone();
        let mut input = json!({
            "commandId": uuid::Uuid::new_v4().to_string(), "threadId": thread_id, "projectId": project_id, "title": title,
            "generateTitle": false, "modelSelection": selection, "runtimeMode": mode, "interactionMode": "default",
            "creationSource": runtime::creation_source(),
            "workspaceStrategy": {"type":"worktree","baseRef":base_ref},
        });
        input["initialMessage"] = json!({"messageId":uuid::Uuid::new_v4().to_string(),"text":text,"attachments":attachments});
        if let Some(context) = &context {
            input["initialMessage"]["context"] = context.clone();
        }
        requests.push(async move {
            let result = if retained.is_some() {
                Err(logic::MAY_HAVE_STARTED.to_owned())
            } else {
                match serde_json::from_value::<t3_contracts::ThreadLaunchInput>(input).and_then(serde_json::to_value) {
                    Ok(input) => runtime::request_value_reported(transport, state, "orchestration.launchThread", input).await.and_then(|receipt| {
                        serde_json::from_value::<ThreadLaunchResult>(receipt).map(|_| ()).map_err(|error| format!("Invalid thread launch receipt: {error}"))
                    }),
                    Err(error) => Err(error.to_string()),
                }
            };
            let may_have_started = retained.is_none() && result.as_ref().err().is_some_and(|error| logic::request_may_have_started(error));
            if may_have_started {
                state.multi_model().write().uncertain.entry(retry_key.clone()).or_insert_with(|| thread_id.clone());
            }
            Outcome { selection, thread_id, retry_key, result }
        });
    }

    // Each request owns its thread once it is out, and the draft is free for another message; the answers are waited for after.
    let mut all = Box::pin(futures_util::future::join_all(requests));
    // The first poll sends every request; whatever it finds finished is kept, and the rest is waited for.
    let first = std::future::poll_fn(|context| std::task::Poll::Ready(std::future::Future::poll(all.as_mut(), context))).await;
    flight.release();
    let outcomes = match first {
        std::task::Poll::Ready(outcomes) => outcomes,
        std::task::Poll::Pending => all.await,
    };

    let mut started = 0;
    let mut failed = vec![];
    for outcome in outcomes {
        match &outcome.result {
            Ok(()) => started += 1,
            Err(error) => {
                let retained = state.multi_model().read().uncertain.get(&outcome.retry_key).cloned();
                tell_failure(state, &environment, &outcome, error, retained);
                failed.push(outcome.selection.clone());
            }
        }
    }
    if started > 0 {
        notice::push(state, notice::Level::Success, logic::started_title(started), "");
    }
    if failed.is_empty() {
        return;
    }
    let taken = Taken { failed, ..taken };
    if can_restore(state, &key, &draft, submitted.as_deref()) {
        put_back(state, &key, &draft, taken);
    } else {
        let token = {
            let mut model = state.multi_model();
            let mut model = model.write();
            model.next_token += 1;
            let token = model.next_token;
            model.lost.insert(token, (key.clone(), taken));
            token
        };
        notice::push_with(
            state,
            notice::Level::Error,
            "A background prompt could not be sent",
            "Your newer draft is unchanged. Restore the failed prompt when this composer is empty.",
            Some(notice::Follow::RestorePrompt { label: "Restore prompt".into(), token }),
        );
    }
}

/// The failure of one model, with a way to the thread its lost request may have started and to allow another send.
fn tell_failure(state: Store<UiModel>, environment: &EnvironmentId, outcome: &Outcome, error: &str, retained: Option<String>) {
    let title = logic::failure_title(outcome.selection.model.as_str());
    match retained {
        Some(thread_id) => {
            notice::push_with_also(
                state,
                notice::Level::Error,
                title,
                error,
                Some(notice::Follow::Thread { label: "Open thread".into(), environment: environment.clone(), thread_id: thread_id.clone() }),
                Some(notice::Follow::AllowRetry { label: "Allow retry".into(), key: outcome.retry_key.clone(), thread_id }),
            );
        }
        None => {
            notice::push(state, notice::Level::Error, title, error);
        }
    }
}

/// `canRestoreDraft`: the same form is open, nothing was written into it since it was cleared, and the models are as submitted.
fn can_restore(state: Store<UiModel>, key: &DraftKey, draft: &DraftTarget, submitted: Option<&[ModelSelection]>) -> bool {
    let open = {
        let model = state.peek();
        model.destination.as_ref() == Some(&key.0) && model.active_draft.as_deref() == Some(key.1.as_str()) && model.active_thread.is_none() && model.view == runtime::View::Chat
    };
    open && draft_is_empty(state, key, draft) && chosen(state, key).as_deref() == submitted
}

fn draft_is_empty(state: Store<UiModel>, key: &DraftKey, draft: &DraftTarget) -> bool {
    state.peek().new_thread_drafts.get(key).is_none_or(|prompt| prompt.is_empty()) && crate::attachments::of_draft(state, draft).is_empty() && crate::composer_chips::count(state, draft) == 0
}

/// The message goes back into the draft and the models that failed stay chosen.
fn put_back(state: Store<UiModel>, key: &DraftKey, draft: &DraftTarget, taken: Taken) {
    crate::draft_storage::edit_draft_prompt(state, key.clone(), taken.prompt);
    crate::attachments::restore(state, draft, taken.attachments);
    crate::composer_chips::edit(state, draft.clone(), taken.chips);
    set_chosen(state, key, Some(taken.failed));
}

/// "Restore prompt" on the notice: back into the form it came from, when that is open and empty.
pub(crate) fn restore_prompt(state: Store<UiModel>, notice_id: u64, token: u64) {
    let Some((key, taken)) = state.multi_model().read().lost.get(&token).cloned() else {
        notice::dismiss(state, notice_id);
        return;
    };
    let draft = DraftTarget::draft(key.0.to_string(), key.1.clone());
    let open = {
        let model = state.peek();
        model.destination.as_ref() == Some(&key.0) && model.active_draft.as_deref() == Some(key.1.as_str()) && model.active_thread.is_none() && model.view == runtime::View::Chat
    };
    if !open || sending(state, &key) || !draft_is_empty(state, &key, &draft) {
        notice::replace(state, notice_id, notice::Level::Error, "A background prompt could not be sent", "Return to the original draft and send or clear its current prompt before restoring.");
        // The button is kept: replacing a notice drops its follow, so it is put back.
        if let Some(notice) = state.notices().write().iter_mut().find(|notice| notice.id == notice_id) {
            notice.follow = Some(notice::Follow::RestorePrompt { label: "Restore prompt".into(), token });
        }
        return;
    }
    state.multi_model().write().lost.remove(&token);
    put_back(state, &key, &draft, taken);
    notice::dismiss(state, notice_id);
}

/// "Allow retry" on a notice: asks first, since the thread may exist.
pub(crate) fn ask_retry(state: Store<UiModel>, notice_id: u64, retry_key: String, thread_id: String) {
    state.multi_model().write().asking = Some((notice_id, retry_key, thread_id));
}

/// The question behind "Allow retry". Yes forgets that the thread may exist, so the next send makes a new one, and closes the notice.
#[component]
pub(crate) fn RetryAsk(state: Store<UiModel>) -> Element {
    let asking = state.multi_model().read().asking.clone();
    let Some((notice_id, retry_key, thread_id)) = asking else { return rsx! {} };
    rsx! {
        crate::settings_common::Confirm { label: "retry", title: "Allow another send?", body: logic::ALLOW_RETRY_QUESTION, confirm: "Allow another send", busy: false,
            oncancel: move |_| state.multi_model().write().asking = None,
            onconfirm: move |_| {
                let mut model = state.multi_model();
                let mut model = model.write();
                model.asking = None;
                if model.uncertain.get(&retry_key) == Some(&thread_id) {
                    model.uncertain.remove(&retry_key);
                    drop(model);
                    notice::dismiss(state, notice_id);
                }
            } }
    }
}
