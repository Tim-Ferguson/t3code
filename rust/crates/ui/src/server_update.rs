//! Updating the connected server from the client, as the original does (`ServerUpdateAction.tsx`, the server-version banner
//! of `ChatView.tsx`, `ComposerServerUpdateStatus.tsx`): a banner above the message box when the server is older than this
//! client, the action it offers (an update the server runs by itself, one the desktop app that runs it carries out after a
//! confirmation, or the command to copy for a server that can do neither), the progress while it runs and the failure it
//! leaves. The rules are `t3_client::server_update`. This server refuses every effect of its own, so the failure state is
//! the one a Rust server most often leaves; a desktop app or launcher that drives it gets the full path.
use crate::{
    composer_banners::{Banner, Variant},
    notice::{self, Level},
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
    settings_common::{Confirm, connected_allows, copy_text},
};
use dioxus::prelude::*;
use serde_json::Value;
use std::collections::BTreeSet;
use t3_client::{
    connection::ConnectionStatus,
    lifecycle as words,
    server_update::{self as logic, Offer, Progress, Stage, State},
};
use t3_contracts::AuthEnvironmentScope;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Model {
    /// The environment the state below belongs to.
    pub environment: Option<String>,
    pub state: State,
    /// Notices the person dismissed (a version pair, by `dismissal_key`), loaded from the browser's storage.
    pub dismissed: BTreeSet<String>,
    pub dismissals_loaded: bool,
    /// A failure the person dismissed; the next attempt shows again.
    pub failure_dismissed: bool,
    /// The question before a desktop app is told to update itself is open, for this target.
    pub confirming: Option<String>,
}

/// What the connected environment says about the server and how it can be updated.
#[derive(Debug, Clone, PartialEq)]
struct Facts {
    environment: String,
    label: String,
    server_version: String,
    offer: Offer,
    continue_threads: bool,
}
fn facts(state: Store<UiModel>) -> Option<Facts> {
    let environment = state.destination().read().as_ref()?.to_string();
    let config = state.config().read().clone();
    let described = &config["environment"];
    Some(Facts {
        environment,
        label: described["label"].as_str().unwrap_or("server").to_owned(),
        server_version: described["serverVersion"].as_str().unwrap_or_default().to_owned(),
        offer: Offer::of(&described["capabilities"]),
        continue_threads: config["settings"]["continueThreadsAfterServerUpdate"] == true,
    })
}

fn can_maintain(state: Store<UiModel>) -> bool {
    connected_allows(state, AuthEnvironmentScope::EnvironmentMaintain)
}

/// The state of the update of this environment; another environment's is not this one's.
fn current(state: Store<UiModel>, environment: &str) -> State {
    let model = state.server_update().read().clone();
    if model.environment.as_deref() == Some(environment) { model.state } else { State::Idle }
}

fn set_state(state: Store<UiModel>, environment: &str, next: State) {
    let mut field = state.server_update();
    let mut model = field.write();
    model.environment = Some(environment.to_owned());
    model.failure_dismissed = false;
    model.state = next;
}

/// Starts the update to `target`, unless one is running or the connection may not maintain the server. A desktop-managed
/// server asks first; `confirmed` is the answer to that.
pub(crate) fn start(state: Store<UiModel>, transport: &TransportHandle, target: &str, confirmed: bool) {
    let Some(facts) = facts(state) else { return };
    if !can_maintain(state) || matches!(current(state, &facts.environment), State::Running { .. }) {
        return;
    }
    if facts.offer.desktop_managed() && !confirmed {
        state.server_update().write().confirming = Some(target.to_owned());
        return;
    }
    state.server_update().write().confirming = None;
    let target = target.to_owned();
    set_state(state, &facts.environment, State::Running { stage: Stage::Downloading, target_version: target.clone() });
    let transport = transport.clone();
    dioxus::dioxus_core::spawn_forever(async move {
        let owner = runtime::response_owner(&transport, state);
        let input = logic::input(&target, facts.offer.thread_continuation && facts.continue_threads);
        let outcome = run(&transport, state, &facts, input).await;
        if owner.is_none() || owner != runtime::response_owner(&transport, state) {
            return;
        }
        match outcome {
            Ok(result) => {
                // The server restarts into the new version: the update is done when this client is connected to it again.
                if facts.offer.desktop_managed() {
                    let (title, detail) = logic::finished(&facts.label, true, &result);
                    notice::push(state, Level::Success, title, detail);
                    set_state(state, &facts.environment, State::Idle);
                } else {
                    set_state(state, &facts.environment, State::Running { stage: Stage::Resuming, target_version: target });
                }
            }
            Err(message) => {
                set_state(state, &facts.environment, State::Failed { message: message.clone(), target_version: target });
                notice::push(state, Level::Error, "Server update failed", message);
            }
        }
    });
}

/// The update itself: the stream with progress where the server offers it, else the plain request; a desktop update is then
/// committed with the token it prepared (the commit relaunches the app, so a connection lost on the way is success).
async fn run(transport: &TransportHandle, state: Store<UiModel>, facts: &Facts, input: Value) -> Result<Value, String> {
    let result = if facts.offer.progress {
        let mut stream = runtime::request_stream_reported(transport, state, logic::UPDATE_WITH_PROGRESS, input)?;
        let mut result = None;
        while let Some(chunk) = stream.next().await {
            for item in chunk? {
                match logic::parse_progress(&item) {
                    Some(Progress::Stage(stage)) => {
                        let target = match current(state, &facts.environment) {
                            State::Running { target_version, .. } => target_version,
                            _ => String::new(),
                        };
                        set_state(state, &facts.environment, State::Running { stage, target_version: target });
                    }
                    Some(Progress::Complete(value)) => result = Some(value),
                    None => {}
                }
            }
        }
        result.ok_or_else(|| "The server stopped before the update finished.".to_owned())?
    } else {
        runtime::request_value_reported(transport.clone(), state, logic::UPDATE, input).await?
    };
    if let Some(token) = logic::desktop_token(&result) {
        match runtime::request_value_reported(transport.clone(), state, logic::COMMIT_DESKTOP_UPDATE, serde_json::json!({"requestId": token})).await {
            Ok(_) => {}
            Err(message) if message.starts_with("Connection closed") || message.contains("interrupted") => {}
            Err(message) => return Err(message),
        }
    }
    Ok(result)
}

/// The banner above the message box, and what finishes an update that was waiting for the server to come back.
#[component]
pub(crate) fn Banners(state: Store<UiModel>, transport: TransportHandle) -> Element {
    // The dismissed notices are read from the browser once.
    use_future(move || async move {
        if state.server_update().peek().dismissals_loaded {
            return;
        }
        let stored = document::eval(&format!("try{{return window.localStorage.getItem({});}}catch{{return null;}}", serde_json::to_string(logic::DISMISSALS_KEY).unwrap_or_default())).join::<Option<String>>().await.ok().flatten();
        let keys: Vec<String> = stored.and_then(|text| serde_json::from_str::<Value>(&text).ok()).and_then(|value| value["keys"].as_array().map(|keys| keys.iter().filter_map(|key| key.as_str().map(str::to_owned)).collect())).unwrap_or_default();
        let mut field = state.server_update();
        let mut model = field.write();
        model.dismissed.extend(keys);
        model.dismissals_loaded = true;
    });
    // A server that was restarting is done when it answers again on the version asked for.
    use_effect(move || {
        let connected = *state.status().read() == ConnectionStatus::Connected;
        let Some(facts) = facts(state) else { return };
        if let State::Running { stage: Stage::Resuming, target_version } = current(state, &facts.environment) {
            if connected && !facts.server_version.is_empty() && logic::resolve_version_mismatch(&target_version, Some(&facts.server_version)).is_none() {
                let (title, detail) = logic::finished(&facts.label, false, &serde_json::json!({"targetVersion": target_version}));
                notice::push(state, Level::Success, title, detail);
                set_state(state, &facts.environment, State::Idle);
            }
        }
    });
    let Some(facts) = facts(state) else { return rsx! {} };
    let model = state.server_update().read().clone();
    let update = current(state, &facts.environment);
    let mismatch = logic::resolve_version_mismatch(logic::app_version(), Some(&facts.server_version));
    let key = mismatch.as_ref().map(|mismatch| logic::dismissal_key(&facts.environment, mismatch));
    let transport_for_confirm = transport.clone();
    let confirm = model.confirming.clone().map(|target| {
        let (title, body) = words::confirm_copy(&logic::confirm_desktop_message(&facts.label));
        let transport = transport_for_confirm.clone();
        rsx! { Confirm { label: "update", title, body: body.unwrap_or_default(), confirm: words::CONFIRM_BUTTON, busy: false,
            oncancel: move |()| state.server_update().write().confirming = None,
            onconfirm: move |()| start(state, &transport, &target, true) } }
    });
    let banner = match &update {
        State::Idle => match (&mismatch, &key) {
            (Some(mismatch), Some(key)) if !model.dismissed.contains(key) => Some(rsx! {
                Banner { variant: Variant::Default, urgent: false, icon: "\u{2193}", title: "Server update available".to_owned(),
                    tooltip: Some(format!("{} {} \u{2192} {}", facts.label, mismatch.server_version, mismatch.client_version)),
                    description: facts.offer.self_update.as_deref().filter(|capability| *capability != "desktop-managed" || !facts.offer.desktop_app_update).map(|capability| logic::guidance(capability).to_owned()),
                    actions: rsx! { Action { state, transport: transport.clone(), facts: facts.clone(), target: mismatch.client_version.clone(), retry: false } },
                    dismiss_label: Some("Dismiss update notice".to_owned()),
                    on_dismiss: { let key = key.clone(); move |()| dismiss_mismatch(state, &key) } }
            }),
            _ => None,
        },
        State::Running { stage, .. } => Some(rsx! {
            Banner { variant: Variant::Default, urgent: true, icon: "\u{27f3}", title: format!("Updating {}", facts.label), detail: Some(stage.label().to_owned()), role: "status", description: None, actions: rsx! {} }
        }),
        State::Failed { message, target_version } if !model.failure_dismissed => Some(rsx! {
            Banner { variant: Variant::Error, urgent: false, icon: "!", title: format!("Could not update {}", facts.label), detail: Some(message.clone()), role: "alert", description: None,
                actions: rsx! { if mismatch.is_some() { Action { state, transport: transport.clone(), facts: facts.clone(), target: target_version.clone(), retry: true } } },
                dismiss_label: Some("Dismiss update notice".to_owned()),
                on_dismiss: { let key = key.clone(); move |()| { state.server_update().write().failure_dismissed = true; if let Some(key) = &key { dismiss_mismatch(state, key); } } } }
        }),
        State::Failed { .. } => None,
    };
    rsx! {
        {banner}
        {confirm}
    }
}

fn dismiss_mismatch(state: Store<UiModel>, key: &str) {
    let keys: Vec<String> = {
        let mut field = state.server_update();
        let mut model = field.write();
        model.dismissed.insert(key.to_owned());
        model.dismissed.iter().cloned().collect()
    };
    let document = serde_json::to_string(&serde_json::json!({"keys": keys})).unwrap_or_default();
    let _ = document::eval(&format!("try{{window.localStorage.setItem({},{});}}catch{{}}", serde_json::to_string(logic::DISMISSALS_KEY).unwrap_or_default(), serde_json::to_string(&document).unwrap_or_default()));
}

/// `ServerUpdateAction`: the button a banner offers.
#[component]
fn Action(state: Store<UiModel>, transport: TransportHandle, facts: Facts, target: String, retry: bool) -> Element {
    if facts.offer.desktop_managed() && !facts.offer.desktop_app_update {
        return rsx! { span { class: "muted", "Update the desktop app on that machine to update this server." } };
    }
    if facts.offer.self_update.is_none() {
        // A server that cannot update itself gets the command that does.
        let command = logic::manual_command(&target, facts.offer.installation.as_ref());
        let label = logic::manual_copy_label(facts.offer.installation.as_ref());
        let (installation, server_label) = (facts.offer.installation.clone(), facts.label.clone());
        return rsx! { button { r#type: "button", "aria-label": "{label} for {facts.label}", onclick: move |_| {
            let (command, installation, server_label) = (command.clone(), installation.clone(), server_label.clone());
            spawn(async move {
                if copy_text(&command).await {
                    let (title, detail) = logic::manual_copied(&command, &server_label, installation.as_ref());
                    notice::push(state, Level::Success, title, detail);
                } else {
                    notice::push(state, Level::Error, "Could not copy update command", "The clipboard is not available.");
                }
            });
        }, "{label}" } };
    }
    let allowed = can_maintain(state);
    let label = if retry { "Retry" } else { "Update" };
    rsx! { button { r#type: "button", "aria-label": "{label} {facts.label}", disabled: !allowed, title: if allowed { "" } else { "This connection may not maintain the server." },
        onclick: move |_| start(state, &transport, &target, false), "{label}" } }
}
