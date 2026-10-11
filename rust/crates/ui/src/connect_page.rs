//! "T3 Connect" on the Connections page: whether this environment is linked to the hosted relay, with the switches for the
//! managed tunnel, for publishing agent activity and for holding webhooks while the environment is offline, and the relay
//! client the tunnel runs with. Port of `CloudLinkRow` / `ConfiguredCloudLinkRow` of `ConnectionsSettings.tsx`,
//! `useCloudLinkController` and the install prompt of `RelayClientInstallDialog.tsx`. The decisions are
//! `t3_client::connect`; this file reads, asks and shows.
//!
//! The state is what `GET /api/connect/link-state` says. The original has no live stream of it either: it reads the state when
//! the section opens and when the window is focused again (a read newer than five seconds is kept), and again after each change
//! made here. This app cannot sign in to the hosted service, so linking is described and not offered (see `t3_client::connect`);
//! the relay client's installation, which the original asks for at the start of linking, is offered on its own row.
use crate::{
    notice,
    runtime::{self, TransportHandle, UiModel},
    settings_common::connected_allows,
};
use dioxus::prelude::*;
use serde_json::json;
use t3_client::connect as logic;
use t3_contracts::AuthEnvironmentScope;

/// A read newer than this is not repeated when the window is focused again (`staleTime` of the original's atom).
const STALE_MS: i64 = 5_000;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[derive(Clone, PartialEq)]
enum Install {
    Idle,
    Confirming(String),
    Installing(String),
}

#[component]
pub(crate) fn Section(state: Store<UiModel>, transport: TransportHandle) -> Element {
    let can_read = connected_allows(state, AuthEnvironmentScope::RelayRead);
    let can_write = connected_allows(state, AuthEnvironmentScope::RelayWrite);
    // The connection this section was read for: a new one is read again.
    let owner = runtime::connection_owner(&transport, state);
    let mut link = use_signal(|| None::<logic::LinkState>);
    let mut read_at = use_signal(|| 0_i64);
    let mut failure = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let mut relay_client = use_signal(|| None::<Result<logic::RelayClient, String>>);
    let mut install = use_signal(|| Install::Idle);

    let reload = use_callback({
        let transport = transport.clone();
        move |(): ()| {
            if !can_read {
                return;
            }
            let transport = transport.clone();
            spawn(async move {
                let answer = runtime::environment_json_with_message(&transport, state, None, logic::LINK_STATE).await;
                match answer.and_then(|value| logic::LinkState::parse(&value)) {
                    Ok(next) => {
                        link.set(Some(next));
                        failure.set(None);
                        read_at.set(now_ms());
                    }
                    Err(message) => failure.set(Some(logic::failure_message(logic::READ_FAILURE, &message))),
                }
            });
        }
    });
    let read_relay_client = use_callback({
        let transport = transport.clone();
        move |(): ()| {
            if !can_read {
                return;
            }
            let transport = transport.clone();
            spawn(async move {
                let answer = runtime::request_value_reported(transport, state, logic::RELAY_CLIENT_STATUS, json!({})).await;
                relay_client.set(Some(answer.and_then(|value| logic::RelayClient::parse(&value))));
            });
        }
    });
    use_resource(use_reactive((&owner, &can_read), move |(owner, can_read)| async move {
        link.set(None);
        relay_client.set(None);
        failure.set(None);
        if owner.is_some() && can_read {
            reload.call(());
            read_relay_client.call(());
        }
    }));
    // The state is read again when the window is focused or shown, unless it was read a moment ago.
    use_future(move || async move {
        let mut watcher = document::eval(
            "let timer=null;const refresh=()=>{clearTimeout(timer);timer=setTimeout(()=>{timer=null;dioxus.send(1);},250);};const visible=()=>{if(document.visibilityState==='visible')refresh();};window.addEventListener('focus',refresh);document.addEventListener('visibilitychange',visible);try{await dioxus.recv();}finally{clearTimeout(timer);window.removeEventListener('focus',refresh);document.removeEventListener('visibilitychange',visible);}",
        );
        while watcher.recv::<i32>().await.is_ok() {
            if now_ms() - *read_at.peek() >= STALE_MS {
                reload.call(());
            }
        }
    });

    if !can_read && !can_write {
        return rsx! {};
    }
    if !can_read {
        return rsx! {
            section { class: "sc-section", id: "connections-environment", "aria-label": "T3 Connect",
                header { h2 { "T3 Connect" } }
                p { class: "muted", "{logic::NO_READ_NOTICE}" }
            }
        };
    }

    // One change at a time; the state is read again when it ends, whichever way.
    let change = use_callback({
        let transport = transport.clone();
        move |(which, enabled): (logic::Switch, bool)| {
            if *busy.peek() {
                return;
            }
            let Some(current) = link.peek().clone() else { return };
            let desired = which.desired(&current, enabled);
            let (path, payload) = match logic::plan(&current, desired) {
                logic::Plan::Unlink => (logic::UNLINK, json!({})),
                logic::Plan::Preferences(body) => (logic::PREFERENCES, body),
                logic::Plan::SignIn => {
                    failure.set(Some(logic::SIGN_IN_REQUIRED.into()));
                    return;
                }
            };
            busy.set(true);
            failure.set(None);
            let transport = transport.clone();
            spawn(async move {
                let answer = runtime::environment_json_with_message(&transport, state, Some(&payload), path).await;
                busy.set(false);
                match answer {
                    Ok(_) => {
                        let (title, detail) = logic::success_message(which, enabled, desired.publish);
                        notice::push(state, notice::Level::Success, title, detail);
                    }
                    Err(message) => {
                        let action = if path == logic::UNLINK { logic::UNLINK_FAILURE } else { logic::PREFERENCES_FAILURE };
                        let message = logic::failure_message(action, &message);
                        notice::push(state, notice::Level::Error, "Could not update T3 Connect", message.clone());
                        failure.set(Some(message));
                    }
                }
                reload.call(());
            });
        }
    });

    // The installation: asked for first, then followed step by step, then its outcome said.
    let run_install = use_callback({
        let transport = transport.clone();
        move |(): ()| {
            let transport = transport.clone();
            spawn(async move {
                let mut stream = match runtime::request_stream_reported(&transport, state, logic::INSTALL_RELAY_CLIENT, json!({})) {
                    Ok(stream) => stream,
                    Err(message) => {
                        install.set(Install::Idle);
                        notice::push(state, notice::Level::Error, "Could not install the relay client", message);
                        return;
                    }
                };
                let mut last = None;
                let mut problem = None;
                while let Some(chunk) = stream.next().await {
                    match chunk {
                        Ok(values) => {
                            for value in values {
                                match logic::install_event(&value) {
                                    Ok(event) => {
                                        if let logic::InstallEvent::Progress(stage) = &event {
                                            install.set(Install::Installing(stage.clone()));
                                        }
                                        last = Some(event);
                                    }
                                    Err(message) => problem = Some(message),
                                }
                            }
                        }
                        Err(message) => {
                            problem = Some(message);
                            break;
                        }
                    }
                }
                install.set(Install::Idle);
                let outcome = match problem {
                    Some(message) => Err(message),
                    None => logic::install_outcome(last.as_ref()),
                };
                match outcome {
                    Ok(()) => notice::push(state, notice::Level::Success, "Relay client installed", "This environment can now run its T3 Connect tunnel."),
                    Err(message) => notice::push(state, notice::Level::Error, "Could not install the relay client", message),
                }
                read_relay_client.call(());
            });
        }
    });

    let current = link.read().clone();
    let busy_now = *busy.read();
    let managed = current.as_ref().is_some_and(|state| state.managed_tunnel());
    let switch = |which: logic::Switch, checked: bool| -> Element {
        let reason = logic::disabled_reason(current.as_ref(), can_write, which, checked);
        let disabled = !can_write || current.is_none() || busy_now || reason.is_some();
        let label = which.label();
        rsx! {
            input { r#type: "checkbox", role: "switch", "aria-label": "{label}", title: reason.unwrap_or_default(), checked, disabled,
                onchange: move |event| change.call((which, event.checked())) }
        }
    };
    let publishing = current.as_ref().is_some_and(|state| state.publish_agent_activity);
    let holding = current.as_ref().is_some_and(|state| state.hold_webhooks());
    let status = match &current {
        None => "Reading the link\u{2026}".to_owned(),
        Some(state) if state.linked => match &state.cloud_user_id {
            Some(user) => format!("Linked to account {user}"),
            None => "Linked".to_owned(),
        },
        Some(_) => "Not linked".to_owned(),
    };
    let client_text = match relay_client.read().clone() {
        None => "Checking the relay client\u{2026}".to_owned(),
        Some(Err(message)) => message,
        Some(Ok(logic::RelayClient::Available)) => "The relay client is installed.".to_owned(),
        Some(Ok(logic::RelayClient::Missing { version })) => format!("The relay client {version} is not installed."),
        Some(Ok(logic::RelayClient::Unsupported { platform, arch })) => logic::RelayClient::unsupported_message(&platform, &arch),
    };
    let missing = matches!(&*relay_client.read(), Some(Ok(logic::RelayClient::Missing { .. })));
    let installing = install.read().clone();
    rsx! {
        section { class: "sc-section", id: "connections-environment", "aria-label": "T3 Connect",
            header { h2 { "T3 Connect" } p { class: "muted", role: "status", "{status}" } }
            if let Some(message) = failure() { p { role: "alert", class: "publish-error", crate::error_text::ErrorText { text: message } } }
            if current.as_ref().is_some_and(|state| !state.linked) {
                p { class: "muted", "Linking an environment needs your T3 Connect account, which this app cannot sign in to. Link it with `t3 connect`, or from the T3 Code app; its state shows here." }
            }
            if let Some(relay) = current.as_ref().and_then(|state| state.relay_url.clone()) { p { class: "muted", "Relay: {relay}" } }
            if crate::keys::desktop() {
                div { class: "setting-row", "data-setting": "t3-connect",
                    div { class: "font-description", strong { "T3 Connect" }
                        p { class: "muted", if managed { "This environment is available to your other devices through T3 Connect." } else { "Make this environment available to your other devices through T3 Connect." } } }
                    div { class: "font-controls", {switch(logic::Switch::ManagedTunnel, managed)} }
                }
            }
            div { class: "setting-row", "data-setting": "publish-agent-activity",
                div { class: "font-description", strong { "Publish agent activity" }
                    p { class: "muted", "Send activity to mobile notifications and Live Activities without T3 Connect." } }
                div { class: "font-controls", {switch(logic::Switch::PublishAgentActivity, publishing)} }
            }
            if managed {
                div { class: "setting-row", "data-setting": "hold-webhooks-while-offline",
                    div { class: "font-description", strong { "Hold webhooks while offline" }
                        p { class: "muted", "Keep webhook requests for up to 24 hours while this environment is offline, then deliver them. Off: T3 Connect only forwards requests and stores nothing." } }
                    div { class: "font-controls", {switch(logic::Switch::HoldWebhooks, holding)} }
                }
            }
            if crate::keys::desktop() {
                div { class: "setting-row", "data-setting": "relay-client",
                    div { class: "font-description", strong { "Relay client" } p { class: "muted", role: "status", "{client_text}" } }
                    div { class: "font-controls",
                        if missing {
                            button { r#type: "button", "aria-label": "Install relay client", disabled: !can_write || installing != Install::Idle,
                                onclick: move |_| {
                                    if let Some(Ok(logic::RelayClient::Missing { version })) = relay_client.peek().clone() {
                                        install.set(Install::Confirming(version));
                                    }
                                }, "Install\u{2026}" }
                        }
                    }
                }
            }
            match installing {
                Install::Idle => rsx! {},
                Install::Confirming(version) => rsx! {
                    div { class: "modal-backdrop", onclick: move |_| install.set(Install::Idle),
                        div { class: "dialog confirm-dialog", role: "alertdialog", "aria-modal": "true", "aria-label": "Install relay client?", tabindex: "-1",
                            onclick: move |event| event.stop_propagation(),
                            onmounted: move |event| { let element = event.data(); spawn(async move { let _ = element.set_focus(true).await; }); },
                            onkeydown: move |event| { if event.key() == Key::Escape { event.stop_propagation(); install.set(Install::Idle); } },
                            h2 { "Install relay client?" }
                            p { "T3 Code needs the relay client to make this environment available through T3 Connect." }
                            p { class: "muted", "T3 Code will download and install version {version} locally." }
                            div { class: "dialog-actions",
                                button { r#type: "button", "aria-label": "Cancel installing the relay client", onclick: move |_| install.set(Install::Idle), "Cancel" }
                                button { class: "primary", r#type: "button", "aria-label": "Download and install", onclick: move |_| { install.set(Install::Installing("checking".into())); run_install.call(()); }, "Download and install" }
                            }
                        }
                    }
                },
                Install::Installing(stage) => {
                    let (label, number) = logic::install_step(&stage).unwrap_or(("Installing relay client", 1));
                    rsx! {
                        div { class: "modal-backdrop",
                            div { class: "dialog confirm-dialog relay-install", role: "alertdialog", "aria-modal": "true", "aria-label": "Installing relay client",
                                h2 { "Installing relay client" }
                                p { "T3 Code is preparing this environment for secure access through T3 Connect." }
                                p { role: "status", "aria-live": "polite", "{label}" }
                                p { class: "muted", "{number} of {logic::INSTALL_STEPS.len()}" }
                                progress { "aria-label": "Relay client installation progress", max: "{logic::INSTALL_STEPS.len()}", value: "{number}" }
                                p { class: "muted", "Keep T3 Code open while the relay client is installed." }
                            }
                        }
                    }
                }
            }
        }
    }
}
