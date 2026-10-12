//! `/feedback` in the composer of a Codex thread (`ChatView.tsx`'s `feedbackCommand`, `submitCodexFeedback`): instead of a message to the
//! agent, the thread's conversation is sent to OpenAI (`provider.uploadFeedback`), and the person is told when it goes and how it ended.
//! One upload per thread runs at a time; the original draws the exchange as two rows of the conversation, this tells it with notices.
use crate::{
    notice::{self, Level},
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
};
use dioxus::prelude::*;
use std::{cell::RefCell, collections::BTreeSet};
use t3_client::thread_feedback as logic;

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
pub(crate) fn submit(handle: &TransportHandle, state: Store<UiModel>, thread_id: &str, reason: Option<String>) {
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
    state.draft().set(String::new());
    crate::draft_storage::edit_thread_prompt(state, String::new());
    notice::push(state, Level::Info, logic::UPLOADING, "");
    let (handle, input) = (handle.clone(), logic::input(thread_id, reason.as_deref()));
    dioxus::dioxus_core::spawn_forever(async move {
        match runtime::request_value_reported(handle, state, logic::METHOD, input).await {
            Ok(result) => {
                let (title, detail) = logic::sent_notice(result["feedbackId"].as_str().unwrap_or_default());
                notice::push(state, Level::Success, title, detail);
            }
            Err(message) => {
                let (title, detail) = logic::failed_notice(&message);
                notice::push(state, Level::Error, title, detail);
            }
        }
        UPLOADING.with(|uploading| uploading.borrow_mut().remove(&key));
    });
}
