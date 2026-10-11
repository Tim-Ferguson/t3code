//! RUST-PORT EXTENSION (the original never imports another server's data). What the person sees about the
//! one-time import of the original app's data (`t3_contracts::state_import`): a notice after the first start that
//! says what came across, what did not and what to enter again, which stays until acknowledged; and the screen
//! of a server that refused to start because the import failed, which serves only `/api/rust/state-import`.
use crate::{
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
    settings_common::connected_allows,
};
use dioxus::prelude::*;
use serde_json::json;
use t3_client::connection::ConnectionStatus;
use t3_contracts::{AuthEnvironmentScope, StateImportOutcome, StateImportStatus};

/// What the failed-import server answers when the app cannot connect: read while disconnected, so a working
/// server is never asked.
pub(crate) fn use_failed_import(state: Store<UiModel>) -> Resource<Option<StateImportStatus>> {
    use_resource(move || {
        // On the desktop the server is the one this app started, at the address it answered on; it is asked when it does.
        let address = probe_address();
        async move {
            if *state.status().read() == ConnectionStatus::Connected {
                return None;
            }
            runtime::probe_state_import(&address?).await.filter(|status| status.status == StateImportOutcome::Failed)
        }
    })
}

fn probe_address() -> Option<String> {
    #[cfg(feature = "desktop")]
    {
        crate::desktop_shell::server_address()
    }
    #[cfg(not(feature = "desktop"))]
    {
        Some(runtime::default_address())
    }
}

#[component]
pub(crate) fn FailedScreen(status: StateImportStatus, retry: EventHandler<()>) -> Element {
    let error = status.error.clone().unwrap_or_else(|| "The import did not say why it stopped.".into());
    rsx! {
        main { class: "import-failed", role: "alert",
            h1 { "Your T3 Code data could not be imported" }
            p { "This server found data of the original T3 Code app and stopped while importing it. Nothing of the original was changed, and nothing is served until this is resolved." }
            pre { class: "import-error", "aria-label": "Import error", "{error}" }
            p { "Fix what the message names and start the server again, or start it with " code { "--no-import" } " to begin without the old data. The import resumes where it stopped when the original's data has not changed." }
            button { r#type: "button", onclick: move |_| retry.call(()), "Try again" }
        }
    }
}

fn plural(count: u64, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// The first-run notice, shown over the page until the person acknowledges it.
#[component]
pub(crate) fn Notice(state: Store<UiModel>, transport: TransportHandle) -> Element {
    let mut pending = use_signal(|| false);
    let mut failure = use_signal(|| None::<String>);
    // The server clears the block for the next config it serves; this window need not wait for one.
    let mut acknowledged = use_signal(|| false);
    let status = state.typed_config().read().as_ref().and_then(|config| config.state_import.clone()).filter(|status| status.status == StateImportOutcome::Imported && !acknowledged());
    let Some(status) = status else { return rsx! {} };
    let allowed = connected_allows(state, AuthEnvironmentScope::SettingsWrite);
    let acknowledge = move |_| {
        if *pending.peek() {
            return;
        }
        pending.set(true);
        failure.set(None);
        let transport = transport.clone();
        spawn(async move {
            let owner = runtime::response_owner(&transport, state);
            let answer = runtime::request_value_reported(transport.clone(), state, t3_contracts::ACKNOWLEDGE_STATE_IMPORT_METHOD, json!({})).await;
            if owner.is_none() || owner != runtime::response_owner(&transport, state) {
                return;
            }
            pending.set(false);
            match answer {
                Ok(_) => acknowledged.set(true),
                Err(message) => failure.set(Some(message)),
            }
        });
    };
    let counts = status.counts.clone();
    rsx! {
        div { class: "modal-backdrop",
            div { class: "dialog import-notice", role: "dialog", "aria-modal": "true", "aria-labelledby": "import-heading", tabindex: "-1",
                h2 { id: "import-heading", "Your T3 Code data was imported" }
                if let Some(counts) = counts {
                    p { "{plural(counts.projects, \"project\", \"projects\")} and {plural(counts.threads, \"thread\", \"threads\")} came across, with their history ({plural(counts.events, \"event\", \"events\")})." 
                        if counts.scheduled_tasks > 0 { " {plural(counts.scheduled_tasks, \"scheduled task\", \"scheduled tasks\")} came across too." } }
                }
                p { class: "muted", "The original app's files were not changed. Work that was running was not carried." }
                if !status.skipped.is_empty() {
                    h3 { "Left behind" }
                    ul { "aria-label": "Not imported",
                        for skipped in status.skipped.iter() { li { key: "{skipped.item}", strong { "{skipped.item}" } " ({plural(skipped.rows, \"row\", \"rows\")}): {skipped.reason}" } }
                    }
                }
                if !status.warnings.is_empty() {
                    h3 { "Notes" }
                    ul { "aria-label": "Import notes", for warning in status.warnings.iter() { li { key: "{warning}", "{warning}" } } }
                }
                if !status.reenter.is_empty() {
                    h3 { "To enter again" }
                    ul { "aria-label": "To enter again", for item in status.reenter.iter() { li { key: "{item}", "{item}" } }
                    }
                }
                if let Some(message) = failure() { p { class: "publish-error", role: "alert", "{message}" } }
                div { class: "dialog-actions",
                    button { class: "primary", r#type: "button", "aria-label": "Got it", disabled: !allowed || pending(), title: if allowed { "" } else { "This connection cannot change settings." }, onclick: acknowledge, if pending() { "Saving\u{2026}" } else { "Got it" } }
                }
            }
        }
    }
}
