//! The server's lifecycle stream on connect: a first run opens the thread it bootstraps, and threads being
//! restored from the previous version are announced until they are. The choices are
//! `t3_client::server_lifecycle`; this follows the stream for the connection it was started on and applies them.
use crate::{
    notice::{self, Level},
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt, View},
};
use dioxus::prelude::*;
use serde_json::json;
use t3_client::server_lifecycle::{self as logic, Action};

/// Follows `subscribeServerLifecycle` for this connection; it ends with the connection or the stream.
pub(crate) fn start(handle: &TransportHandle, state: Store<UiModel>) {
    let Some(owner) = runtime::connection_owner(handle, state) else { return };
    let Ok(mut wire) = runtime::request_stream(handle, state, "subscribeServerLifecycle", json!({})) else { return };
    let handle = handle.clone();
    spawn(async move {
        while let Some(values) = wire.next().await {
            if runtime::connection_owner(&handle, state).as_ref() != Some(&owner) {
                return;
            }
            let Ok(values) = values else { return };
            for event in values {
                apply(&handle, state, &event);
            }
        }
    });
}

pub(crate) fn apply(handle: &TransportHandle, state: Store<UiModel>, event: &serde_json::Value) {
    if let Some(welcome) = t3_client::onboarding::Welcome::from_event(event) {
        state.server_welcome().set(Some(welcome));
    }
    let (handled, on_start_screen) = {
        let model = state.peek();
        (model.bootstrap_opened.clone(), model.view == View::Chat && model.active_thread.is_none())
    };
    for action in logic::actions(event, handled.as_deref(), on_start_screen) {
        match action {
            Action::OpenBootstrapThread(thread) => {
                state.bootstrap_opened().set(Some(thread.clone()));
                runtime::select_thread(handle, state, thread);
            }
            Action::ShowMigration(text) => {
                // One notice for the whole restoration: a newer count replaces the words, not the notice.
                clear_migration(state);
                notice::push(state, Level::Info, logic::MIGRATION_TITLE, text);
                let id = state.notices().peek().last().map(|notice| notice.id);
                state.migration_notice().set(id);
            }
            Action::ClearMigration => clear_migration(state),
        }
    }
}

fn clear_migration(state: Store<UiModel>) {
    let shown = state.migration_notice().peek().clone();
    if let Some(id) = shown {
        state.notices().write().retain(|notice| notice.id != id);
        state.migration_notice().set(None);
    }
}
