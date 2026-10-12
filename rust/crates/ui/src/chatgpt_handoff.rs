//! Signing a remote environment's managed Codex in to ChatGPT through this computer's own server (`CodexSetupSection.tsx`, the
//! `primaryAuthEnvironmentId` path). The sign-in page redirects to a loopback address, which only this computer can reach, so the
//! environment on this computer does the OAuth for the remote one and hands the result over:
//!
//! 1. `provider.chatgpt.reconnect-profile` asks the remote environment for the registration to sign in again with (none the first time);
//! 2. `provider.chatgpt.handoff.subscribe` on the local environment runs the sign-in (its states, with the page to open) and ends with the
//!    profile it made, whose tokens exist only in that stream's memory;
//! 3. `provider.chatgpt.import-profile` on the remote environment keeps the profile.
//!
//! Each request goes out on the connection of its own environment, so the person can be looking at either one meanwhile.
use crate::{
    fleet,
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
};
use dioxus::prelude::*;
use serde_json::{Value, json};
use t3_client::{
    chatgpt::{Callback, Handoff},
    connection::ConnectionStatus,
};
use t3_contracts::{AuthEnvironmentScope, EnvironmentId};

#[cfg(test)]
thread_local! {
    /// Environments a test says are not on this computer, though its fixture servers all listen on loopback.
    pub(crate) static REMOTE: std::cell::RefCell<std::collections::BTreeSet<EnvironmentId>> = const { std::cell::RefCell::new(std::collections::BTreeSet::new()) };
}

/// Whether the environment's address is on this computer.
fn on_this_computer(link: &TransportHandle, environment: &EnvironmentId) -> bool {
    #[cfg(test)]
    if REMOTE.with(|remote| remote.borrow().contains(environment)) {
        return false;
    }
    let _ = environment;
    runtime::connected_to_loopback(link)
}

/// Whether this page runs on the computer the local environment is on: the desktop app and a native window do, a browser does
/// when it was loaded from a loopback address.
fn page_on_this_computer() -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window().and_then(|window| window.location().hostname().ok()).is_some_and(|host| matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]" | "::1"))
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        true
    }
}

/// The two connections of a sign-in through this computer.
#[derive(Clone)]
pub(crate) struct Route {
    pub local: TransportHandle,
    pub remote: TransportHandle,
    pub remote_environment: EnvironmentId,
}

/// `primaryAuthEnvironmentId`: the local environment that signs in for `remote`, when it should. Not for an environment that is on this
/// computer (its own server receives the redirect), nor without a connected local environment this session may manage providers on.
pub(crate) fn route(transport: &TransportHandle, state: Store<UiModel>, remote: &EnvironmentId) -> Option<Route> {
    let remote_link = transport.link_of(remote)?;
    let local = state
        .live()
        .peek()
        .keys()
        .filter(|environment| *environment != remote)
        .find(|environment| {
            transport.link_of(environment).is_some_and(|link| on_this_computer(&link, environment))
                && fleet::status(state, environment) == ConnectionStatus::Connected
                && fleet::allows(state, environment, AuthEnvironmentScope::ProvidersManage)
        })
        .and_then(|environment| transport.link_of(environment))?;
    t3_client::chatgpt::handoff_through_local_server(on_this_computer(&remote_link, remote), true, page_on_this_computer()).then(|| Route { local, remote: remote_link, remote_environment: remote.clone() })
}

/// Where the sign-in stands, for the account section to draw.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Progress {
    /// The local environment's sign-in state: the page to open is in it.
    Auth(Value),
    /// The profile is made and is being kept by the remote environment.
    Importing,
    Done,
    Failed(String),
}

/// Runs the three steps; `report` hears each. The sign-in ends when this future is dropped (the stream to the local environment
/// is cancelled, which ends the flow there).
pub(crate) async fn run(state: Store<UiModel>, route: Route, instance: String, method: String, return_url: String, mut report: impl FnMut(Progress)) {
    let profile = match runtime::request_value_reported(route.remote.clone(), state, "provider.chatgpt.reconnect-profile", json!({"instanceId":instance,"methodId":method})).await {
        Ok(profile) => profile,
        Err(cause) => return report(Progress::Failed(cause)),
    };
    let input = json!({"instanceId":instance,"environmentId":route.remote_environment,"attemptId":uuid::Uuid::new_v4().to_string(),"returnUrl":return_url,"profile":profile});
    let mut stream = match runtime::request_stream_link_reported(&route.local, state, "provider.chatgpt.handoff.subscribe", input) {
        Ok(stream) => stream,
        Err(cause) => return report(Progress::Failed(cause)),
    };
    let mut fold = Handoff::Waiting;
    loop {
        let items = match stream.next().await {
            Some(Ok(items)) => items,
            Some(Err(cause)) => return report(Progress::Failed(cause)),
            None => {
                // The stream ended before a profile arrived.
                if let Handoff::Failed(message) = fold.interrupted() {
                    report(Progress::Failed(message));
                }
                return;
            }
        };
        for item in items {
            fold = fold.apply(&item);
            match &fold {
                Handoff::Signing { .. } => report(Progress::Auth(item["state"].clone())),
                Handoff::Failed(message) => return report(Progress::Failed(message.clone())),
                Handoff::Finished { profile } => {
                    let profile = profile.clone();
                    drop(stream);
                    return finish(state, &route, &instance, profile, &mut report).await;
                }
                Handoff::Waiting => {}
            }
        }
    }
}

async fn finish(state: Store<UiModel>, route: &Route, instance: &str, profile: Value, report: &mut impl FnMut(Progress)) {
    report(Progress::Importing);
    match runtime::request_value_reported(route.remote.clone(), state, "provider.chatgpt.import-profile", json!({"instanceId":instance,"profile":profile})).await {
        Ok(_) => report(Progress::Done),
        Err(cause) => report(Progress::Failed(cause)),
    }
}

/// `provider.codex.auth-callback.subscribe`: a sign-in that the remote environment itself started (`provider.auth.start`, the redirect
/// brought back by the client) has its redirect received by the local environment's loopback listener, instead of the person pasting
/// it. Returns the redirect's address once it has arrived, for `provider.auth.complete` on the remote environment. Only the remote
/// environment ever holds tokens. `listening` is called once the listener is up, which is when the sign-in page is worth opening.
pub(crate) async fn receive_redirect(state: Store<UiModel>, route: &Route, instance: &str, flow_id: &str, authorization_url: &str, return_url: &str, listening: impl FnOnce()) -> Result<String, String> {
    let input = json!({"authorizationUrl":authorization_url,"returnUrl":return_url,"environmentId":route.remote_environment,"instanceId":instance,"flowId":flow_id});
    let mut stream = runtime::request_stream_link_reported(&route.local, state, "provider.codex.auth-callback.subscribe", input)?;
    let (mut fold, mut listening) = (Callback::Waiting, Some(listening));
    while let Some(items) = stream.next().await {
        for item in items? {
            fold = fold.apply(&item);
            match &fold {
                Callback::Listening => {
                    if let Some(listening) = listening.take() {
                        listening();
                    }
                }
                Callback::Received(address) => return Ok(address.clone()),
                Callback::Waiting => {}
            }
        }
    }
    Err("The sign-in on this computer was interrupted. Try again or paste the redirect URL.".into())
}
