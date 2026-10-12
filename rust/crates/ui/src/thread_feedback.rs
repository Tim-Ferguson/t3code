//! `/feedback` in the composer of a Codex thread (`ChatView.tsx`'s `feedbackCommand`, `submitCodexFeedback`, `ComposerFeedback.tsx`): instead of a
//! message to the agent, the thread's conversation is sent to OpenAI (`provider.uploadFeedback`). The exchange is a banner above the
//! composer (sending, then sent with a way to copy the thread ID, or failed, each dismissible once it is over) and two rows of the
//! conversation that exist only on this client: what was typed, and the answer. One upload per thread runs at a time.
use crate::{
    composer_banners::{Banner, Variant},
    notice::{self, Level},
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
    sidebar_rows::copy_text,
};
use dioxus::prelude::*;
use std::{cell::RefCell, collections::{BTreeMap, BTreeSet}};
use t3_client::thread_feedback::{self as logic, Status, Submission};

/// The submissions of each thread (`environment`, `thread`), oldest first.
pub type Submissions = BTreeMap<(String, String), Vec<Submission>>;

thread_local! {
    static UPLOADING: RefCell<BTreeSet<(String, String)>> = const { RefCell::new(BTreeSet::new()) };
}

/// Whether the thread's model runs on the Codex driver, the only one whose conversation OpenAI takes.
pub(crate) fn is_codex(state: Store<UiModel>, selection: &t3_contracts::ModelSelection) -> bool {
    let field = state.typed_config();
    let config = field.read();
    config.as_ref().is_some_and(|config| config.providers.0.iter().any(|provider| provider.instance_id == selection.instance_id && provider.driver.as_str() == "codex"))
}

/// Sends the thread's feedback. The command leaves the composer as a message does; a thread that has not started a Codex conversation
/// has nothing to send and keeps it.
pub(crate) fn submit(handle: &TransportHandle, state: Store<UiModel>, thread_id: &str, command: String, reason: Option<String>) {
    let started = state.peek().shell.snapshot.as_ref().is_some_and(|snapshot| snapshot.threads.iter().any(|thread| thread.id.as_str() == thread_id && thread.active_provider_thread_id.is_some()));
    if !started {
        notice::push(state, Level::Warning, logic::START_FIRST.0, logic::START_FIRST.1);
        return;
    }
    let Some(environment) = state.destination().peek().as_ref().map(ToString::to_string) else { return };
    let key = (environment, thread_id.to_owned());
    if !UPLOADING.with(|uploading| uploading.borrow_mut().insert(key.clone())) {
        return;
    }
    let id = uuid::Uuid::new_v4().to_string();
    let created_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    state.feedback().write().entry(key.clone()).or_default().push(Submission { id: id.clone(), command, created_at, status: Status::Uploading });
    state.draft().set(String::new());
    crate::draft_storage::edit_thread_prompt(state, String::new());
    let (handle, input) = (handle.clone(), logic::input(thread_id, reason.as_deref()));
    dioxus::dioxus_core::spawn_forever(async move {
        let status = match runtime::request_value_reported(handle, state, logic::METHOD, input).await {
            Ok(result) => Status::Sent { feedback_id: result["feedbackId"].as_str().unwrap_or_default().to_owned() },
            Err(error) => Status::Failed { error },
        };
        if let Some(submission) = state.feedback().write().get_mut(&key).and_then(|list| list.iter_mut().find(|submission| submission.id == id)) {
            submission.status = status;
        }
        UPLOADING.with(|uploading| uploading.borrow_mut().remove(&key));
    });
}

fn submissions(state: Store<UiModel>, thread_id: &str) -> Vec<Submission> {
    let Some(environment) = state.destination().read().as_ref().map(ToString::to_string) else { return vec![] };
    state.feedback().read().get(&(environment, thread_id.to_owned())).cloned().unwrap_or_default()
}

/// The banners of a thread's feedback (`feedbackBannerItem`), for the stack above the composer.
#[component]
pub(crate) fn Banners(state: Store<UiModel>, thread_id: String) -> Element {
    let list = submissions(state, &thread_id);
    rsx! {
        for submission in list {
            {
                let (title, description) = logic::notice(&submission);
                let uploading = submission.status == Status::Uploading;
                let (id, thread) = (submission.id.clone(), thread_id.clone());
                let copy = match &submission.status { Status::Sent { feedback_id } => Some(feedback_id.clone()), _ => None };
                let variant = match submission.status { Status::Failed { .. } => Variant::Error, Status::Sent { .. } => Variant::Success, Status::Uploading => Variant::Info };
                rsx! { Banner { key: "feedback:{submission.id}", variant, urgent: false, icon: "\u{1f4ac}".to_owned(), title, description,
                    actions: rsx! { if let Some(feedback_id) = copy { button { r#type: "button", "aria-label": "Copy ID", onclick: move |_| copy_text(&feedback_id), "Copy ID" } } },
                    dismiss_label: (!uploading).then(|| "Dismiss feedback notice".to_owned()),
                    on_dismiss: move |_| {
                        let Some(environment) = state.destination().peek().as_ref().map(ToString::to_string) else { return };
                        if let Some(list) = state.feedback().write().get_mut(&(environment, thread.clone())) { list.retain(|entry| entry.id != id); }
                    } } }
            }
        }
    }
}

/// The two rows of the conversation each submission makes (`codexFeedbackMessage`): what was typed, and the answer.
#[component]
pub(crate) fn Rows(state: Store<UiModel>, thread_id: String) -> Element {
    let list = submissions(state, &thread_id);
    rsx! {
        for submission in list {
            div { key: "feedback-rows:{submission.id}", class: "feedback-rows", "data-timeline-row": "feedback:{submission.id}",
                crate::Message { role: "user".to_owned(), text: logic::message_text(&submission, false) }
                crate::Message { role: "assistant".to_owned(), text: logic::message_text(&submission, true) }
            }
        }
    }
}
