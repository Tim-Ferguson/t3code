//! "SSH environments" on the Connections page and the password dialog: add a host the local ssh reaches, connect to it
//! (the desktop shell launches or reuses a T3 Code server there and forwards its port), switch the window to it, and
//! disconnect. Port of the SSH tab of the add-environment dialog of `ConnectionsSettings.tsx`, `SshPasswordPromptDialog.tsx`
//! and the `provision`/`prepare`/`disconnect` of `SshEnvironmentGateway` (`apps/web/src/connection/platform.ts`).
//!
//! Only the desktop app can run ssh: the shell provides a [`Gateway`] as a context, and without one (the web build) the page
//! shows nothing of this. This app holds one connection at a time, so "connected" here means the window is switched to the remote
//! environment, and "Use this computer" switches it back. The decisions are `t3_client::ssh`.
use crate::runtime::{UiModel, UiModelStoreExt};
use dioxus::prelude::*;
use t3_client::ssh::Target;
use t3_contracts::EnvironmentId;

/// What the window is doing with an SSH environment now.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Phase {
    #[default]
    Idle,
    /// The tunnel is being made (the remote server launched or reused, a pairing credential issued).
    Preparing(Target),
    /// The credential is being exchanged and the window connected.
    Connecting(Target),
    Disconnecting(Target),
}

/// The environment the window was switched to through ssh.
#[derive(Debug, Clone, PartialEq)]
pub struct Active {
    pub target: Target,
    pub environment: EnvironmentId,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Model {
    /// The hosts added, kept by the shell.
    pub saved: Vec<Target>,
    pub phase: Phase,
    pub active: Option<Active>,
    pub error: Option<String>,
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) use native::{Ensured, Gateway, GatewayHandle, Host, Local, Primary, Section};

#[cfg(target_arch = "wasm32")]
#[component]
pub(crate) fn Section(state: Store<UiModel>, transport: crate::runtime::TransportHandle) -> Element {
    let _ = (state, transport);
    rsx! {}
}
#[cfg(target_arch = "wasm32")]
#[component]
pub(crate) fn Host(state: Store<UiModel>, transport: crate::runtime::TransportHandle) -> Element {
    let _ = (state, transport);
    rsx! {}
}

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use super::*;
    use crate::{
        notice,
        runtime::{self, TransportHandle},
    };
    use std::{future::Future, pin::Pin, rc::Rc};
    use t3_client::{
        connection::ConnectionStatus,
        ssh::{self as logic, DiscoveredHost, PromptRequest},
    };

    pub(crate) type Local<T> = Pin<Box<dyn Future<Output = T>>>;

    /// What preparing a tunnel came to.
    pub(crate) enum Ensured {
        Ready { http_base_url: String, pairing_token: Option<String> },
        /// The person refused (or let time run out on) the password prompt.
        Cancelled(String),
    }

    /// How to reach the server the desktop started for itself again.
    pub(crate) enum Primary {
        Pair { address: String, credential: String },
        Open { address: String },
    }

    /// What the desktop shell does for this page (`window.desktopBridge` of the original).
    pub(crate) trait Gateway {
        fn discover_hosts(&self) -> Local<Vec<DiscoveredHost>>;
        fn resolve_host(&self, alias: String) -> Local<Result<Target, String>>;
        fn ensure(&self, target: Target) -> Local<Result<Ensured, String>>;
        fn disconnect(&self, target: Target) -> Local<Result<(), String>>;
        /// The password questions ssh has asked, oldest first.
        fn prompts(&self) -> tokio::sync::watch::Receiver<Vec<PromptRequest>>;
        /// The person's answer to one, or none for cancelling it.
        fn answer_prompt(&self, request_id: String, password: Option<String>) -> Result<(), String>;
        fn primary(&self) -> Option<Primary>;
        fn saved(&self) -> Vec<Target>;
        fn save(&self, targets: &[Target]);
    }

    #[derive(Clone)]
    pub(crate) struct GatewayHandle(pub Rc<dyn Gateway>);
    impl PartialEq for GatewayHandle {
        fn eq(&self, other: &Self) -> bool {
            Rc::ptr_eq(&self.0, &other.0)
        }
    }

    fn gateway() -> Option<Rc<dyn Gateway>> {
        try_consume_context::<GatewayHandle>().map(|handle| handle.0)
    }

    fn fail(mut state: Store<UiModel>, message: &str) {
        let mut whole = state.write();
        let model = &mut whole.ssh;
        model.error = Some(logic::format_connection_error(message));
        model.phase = Phase::Idle;
    }

    /// Switches the window back to the server this app started.
    fn use_primary(gateway: &Rc<dyn Gateway>, transport: &TransportHandle, state: Store<UiModel>) {
        let Some(primary) = gateway.primary() else { return };
        runtime::begin_connection(transport, state);
        let transport = transport.clone();
        spawn(async move {
            match primary {
                Primary::Pair { address, credential } => runtime::pair_and_connect(transport, state, address, credential).await,
                Primary::Open { address } => runtime::connect(transport, state, address, String::new()).await,
            }
        });
    }

    /// `provisionDesktopSshEnvironment`: prepare the tunnel and a pairing credential, then pair with the environment through it.
    fn start_connect(gateway: Rc<dyn Gateway>, transport: TransportHandle, mut state: Store<UiModel>, target: Target) {
        if state.peek().ssh.phase != Phase::Idle {
            return;
        }
        {
            let mut whole = state.write();
        let model = &mut whole.ssh;
            model.error = None;
            model.phase = Phase::Preparing(target.clone());
        }
        spawn(async move {
            match gateway.ensure(target.clone()).await {
                Err(message) | Ok(Ensured::Cancelled(message)) => fail(state, &message),
                Ok(Ensured::Ready { pairing_token: None, .. }) => fail(state, "The SSH environment did not issue a pairing credential."),
                Ok(Ensured::Ready { http_base_url, pairing_token: Some(credential) }) => {
                    runtime::begin_connection(&transport, state);
                    state.ssh().write().phase = Phase::Connecting(target);
                    // Returns when this connection is replaced; `Host` sees the outcome in the window's status.
                    runtime::pair_and_connect(transport, state, http_base_url, credential).await;
                }
            }
        });
    }

    fn start_disconnect(gateway: Rc<dyn Gateway>, transport: TransportHandle, mut state: Store<UiModel>, target: Target) {
        if state.peek().ssh.phase != Phase::Idle {
            return;
        }
        {
            let mut whole = state.write();
        let model = &mut whole.ssh;
            model.error = None;
            model.phase = Phase::Disconnecting(target.clone());
        }
        spawn(async move {
            // Leave the tunnel before it closes, so the window is not left reconnecting to a port nobody listens on.
            if viewing(state, &target) {
                use_primary(&gateway, &transport, state);
            }
            match gateway.disconnect(target.clone()).await {
                Err(message) => fail(state, &message),
                Ok(()) => {
                    let mut whole = state.write();
        let model = &mut whole.ssh;
                    if model.active.as_ref().is_some_and(|active| active.target == target) {
                        model.active = None;
                    }
                    model.phase = Phase::Idle;
                    drop(whole);
                    notice::push(state, notice::Level::Success, "Environment disconnected", format!("{} was disconnected.", target.alias));
                }
            }
        });
    }

    /// Whether the window shows the environment reached through this target.
    fn viewing(state: Store<UiModel>, target: &Target) -> bool {
        let model = state.peek();
        model.status == ConnectionStatus::Connected && model.ssh.active.as_ref().is_some_and(|active| &active.target == target && model.destination.as_ref() == Some(&active.environment))
    }

    #[component]
    pub(crate) fn Section(state: Store<UiModel>, transport: TransportHandle) -> Element {
        let Some(gateway) = gateway() else { return rsx! {} };
        let mut hosts = use_signal(|| None::<Vec<DiscoveredHost>>);
        let mut host = use_signal(String::new);
        let mut username = use_signal(String::new);
        let mut port = use_signal(String::new);
        let mut form_error = use_signal(|| None::<String>);
        // The hosts are read each time the section is shown: the ssh configuration may have changed.
        use_future({
            let gateway = gateway.clone();
            move || {
                let gateway = gateway.clone();
                async move { hosts.set(Some(gateway.discover_hosts().await)) }
            }
        });
        // A new connection empties the form, as the original's dialog closes.
        use_effect(move || {
            if state.ssh().read().active.is_some() {
                host.set(String::new());
                username.set(String::new());
                port.set(String::new());
            }
        });
        let model = state.ssh().read().clone();
        let busy = model.phase != Phase::Idle;
        let adding = matches!(model.phase, Phase::Preparing(_) | Phase::Connecting(_));
        let connected_now = model.active.as_ref().filter(|active| viewing(state, &active.target)).map(|active| active.target.clone());
        let suggestions: Vec<DiscoveredHost> = hosts()
            .map(|all| {
                let unsaved: Vec<DiscoveredHost> = all.into_iter().filter(|candidate| !model.saved.iter().any(|saved| saved.alias == candidate.target.alias || logic::format_target(saved) == logic::format_target(&candidate.target))).collect();
                logic::filter_hosts(&unsaved, &host()).into_iter().cloned().collect()
            })
            .unwrap_or_default();
        let message = form_error().or(model.error.clone());
        let add = {
            let (gateway, transport) = (gateway.clone(), transport.clone());
            move |_| match logic::parse_manual_target(&host(), &username(), &port()) {
                Ok(target) => {
                    form_error.set(None);
                    start_connect(gateway.clone(), transport.clone(), state, target);
                }
                Err(message) => form_error.set(Some(logic::format_connection_error(&message))),
            }
        };
        rsx! {
            section { class: "sc-section ssh-section", "aria-label": "SSH environments",
                h2 { "SSH environments" }
                p { class: "muted", "Use local SSH config, agent, and tunnels for the backend." }
                for target in model.saved.iter().cloned() {
                    {
                        let alias = target.alias.clone();
                        let is_active = model.active.as_ref().is_some_and(|active| active.target == target);
                        let here = connected_now.as_ref() == Some(&target);
                        rsx! {
                            article { class: "connection-row", key: "ssh:{alias}",
                                div {
                                    strong { "{alias}" }
                                    if here { span { class: "muted", " \u{b7} Connected" } } else if is_active { span { class: "muted", " \u{b7} Tunnel open" } }
                                    p { class: "muted", {logic::format_target(&target)} }
                                }
                                button { r#type: "button", "aria-label": "Connect {alias}", disabled: busy || here,
                                    onclick: { let (gateway, transport, target) = (gateway.clone(), transport.clone(), target.clone()); move |_| start_connect(gateway.clone(), transport.clone(), state, target.clone()) },
                                    if matches!(&model.phase, Phase::Preparing(current) | Phase::Connecting(current) if *current == target) { "Connecting\u{2026}" } else { "Connect" }
                                }
                                if is_active {
                                    button { r#type: "button", "aria-label": "Disconnect {alias}", disabled: busy,
                                        onclick: { let (gateway, transport, target) = (gateway.clone(), transport.clone(), target.clone()); move |_| start_disconnect(gateway.clone(), transport.clone(), state, target.clone()) },
                                        "Disconnect"
                                    }
                                } else {
                                    button { r#type: "button", "aria-label": "Remove {alias}", disabled: busy,
                                        onclick: { let (gateway, target) = (gateway.clone(), target.clone()); move |_| {
                                            let remaining: Vec<Target> = state.ssh().read().saved.iter().filter(|saved| **saved != target).cloned().collect();
                                            gateway.save(&remaining);
                                            state.ssh().write().saved = remaining;
                                        } },
                                        "Remove"
                                    }
                                }
                            }
                        }
                    }
                }
                if connected_now.is_some() && gateway.primary().is_some() {
                    button { r#type: "button", "aria-label": "Use this computer", disabled: busy,
                        onclick: { let (gateway, transport) = (gateway.clone(), transport.clone()); move |_| use_primary(&gateway, &transport, state) },
                        "Use this computer"
                    }
                }
                div { class: "ssh-fields",
                    label { r#for: "ssh-host", "SSH host or alias" }
                    input { id: "ssh-host", "aria-label": "SSH host or alias", placeholder: "Search hosts or type devbox", spellcheck: "false", disabled: adding, value: "{host}", oninput: move |event| host.set(event.value()) }
                    if hosts().is_none() { p { class: "muted", "Loading hosts\u{2026}" } }
                    else if !suggestions.is_empty() {
                        ul { class: "ssh-hosts", "aria-label": "Discovered SSH hosts",
                            for candidate in suggestions {
                                {
                                    let alias = candidate.target.alias.clone();
                                    let address = logic::format_target(&candidate.target);
                                    rsx! {
                                        li { key: "host:{alias}:{address}",
                                            button { r#type: "button", "aria-label": "Use host {alias}", disabled: adding,
                                                // Picking a suggestion resolves the alias, as `ssh -G` reads it, and connects.
                                                onclick: { let (gateway, transport, alias) = (gateway.clone(), transport.clone(), alias.clone()); move |_| {
                                                    let (gateway, transport, alias) = (gateway.clone(), transport.clone(), alias.clone());
                                                    host.set(alias.clone());
                                                    form_error.set(None);
                                                    spawn(async move {
                                                        match gateway.resolve_host(alias).await {
                                                            Ok(resolved) => {
                                                                username.set(resolved.username.clone().unwrap_or_default());
                                                                port.set(resolved.port.map(|port| port.to_string()).unwrap_or_default());
                                                                start_connect(gateway, transport, state, resolved);
                                                            }
                                                            Err(message) => form_error.set(Some(logic::format_connection_error(&message))),
                                                        }
                                                    });
                                                } },
                                                strong { "{alias}" }
                                                if address != alias { span { class: "muted", " {address}" } }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    } else if !host().trim().is_empty() && hosts().is_some_and(|all| !all.is_empty()) {
                        p { class: "muted", "No hosts match \"{host().trim()}\"." }
                    }
                    label { r#for: "ssh-username", "Username" }
                    input { id: "ssh-username", "aria-label": "SSH username", placeholder: "root", spellcheck: "false", disabled: adding, value: "{username}", oninput: move |event| username.set(event.value()) }
                    label { r#for: "ssh-port", "Port" }
                    input { id: "ssh-port", "aria-label": "SSH port", placeholder: "22", inputmode: "numeric", spellcheck: "false", disabled: adding, value: "{port}", oninput: move |event| port.set(event.value()) }
                    if let Some(message) = message { p { role: "alert", class: "publish-error", crate::error_text::ErrorText { text: message } } }
                    button { r#type: "button", "aria-label": "Add environment", disabled: busy, onclick: add,
                        if adding { "Adding\u{2026}" } else { "Add environment" }
                    }
                }
            }
        }
    }

    /// Always mounted: the password dialog, and the bookkeeping that follows the window's connection once a switch to an SSH
    /// environment has been started from anywhere.
    #[component]
    pub(crate) fn Host(state: Store<UiModel>, transport: TransportHandle) -> Element {
        let _ = &transport;
        let Some(gateway) = gateway() else { return rsx! {} };
        let mut prompts = use_signal(Vec::<PromptRequest>::new);
        use_future({
            let gateway = gateway.clone();
            move || {
                let gateway = gateway.clone();
                async move {
                    state.ssh().write().saved = gateway.saved();
                    let mut feed = gateway.prompts();
                    loop {
                        prompts.set(feed.borrow_and_update().clone());
                        if feed.changed().await.is_err() {
                            return;
                        }
                    }
                }
            }
        });
        // The outcome of a switch: the window connected to the environment, or it could not.
        use_effect({
            let gateway = gateway.clone();
            move || {
                let phase = state.ssh().read().phase.clone();
                let Phase::Connecting(target) = phase else { return };
                let status = state.status().read().clone();
                match status {
                    ConnectionStatus::Connected => {
                        let Some(environment) = state.destination().read().clone() else { return };
                        let saved = {
                            let mut whole = state.write();
        let model = &mut whole.ssh;
                            model.active = Some(Active { target: target.clone(), environment });
                            model.phase = Phase::Idle;
                            if !model.saved.iter().any(|saved| *saved == target) {
                                model.saved.push(target.clone());
                            }
                            model.saved.clone()
                        };
                        gateway.save(&saved);
                        notice::push(state, notice::Level::Success, "Environment connected", format!("{} is ready over an SSH-managed tunnel.", target.alias));
                    }
                    ConnectionStatus::Blocked(message) | ConnectionStatus::Interrupted(message) => fail(state, &message),
                    ConnectionStatus::Connecting | ConnectionStatus::Disconnected => {}
                }
            }
        });
        let first = prompts.read().first().cloned();
        rsx! {
            if let Some(request) = first {
                ActivePrompt { key: "{request.request_id}", gateway: GatewayHandle(gateway.clone()), request }
            }
        }
    }

    fn now_ms() -> u64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_millis() as u64)
    }

    /// `ActiveSshPasswordPrompt`.
    #[component]
    fn ActivePrompt(gateway: GatewayHandle, request: PromptRequest) -> Element {
        let mut password = use_signal(String::new);
        let mut responding = use_signal(|| false);
        let mut answer_error = use_signal(|| None::<String>);
        let mut now = use_signal(now_ms);
        // The countdown ticks once a second for as long as the dialog is open, and not otherwise.
        use_future(move || async move {
            loop {
                runtime::delay(1_000).await;
                now.set(now_ms());
            }
        });
        let left = logic::seconds_left(&request, now());
        let expired = left == 0;
        let target = logic::describe_prompt_target(&request);
        let shown_error = if expired { Some(logic::EXPIRED_MESSAGE.to_owned()) } else { answer_error() };
        let respond = {
            let (gateway, request_id) = (gateway.clone(), request.request_id.clone());
            use_callback(move |answer: Option<String>| {
                if responding() {
                    return;
                }
                if answer.is_some() && now_ms() >= request.expires_at_ms {
                    answer_error.set(Some(logic::EXPIRED_MESSAGE.to_owned()));
                    return;
                }
                responding.set(true);
                answer_error.set(None);
                let cancelling = answer.is_none();
                // The dialog goes away when the shell's queue drops the question, which the answer does at once.
                if let Err(message) = gateway.0.answer_prompt(request_id.clone(), answer) {
                    if !cancelling {
                        answer_error.set(Some(logic::prompt_error_message(&message)));
                    }
                }
                responding.set(false);
            })
        };
        rsx! {
            div { class: "modal-backdrop",
                div { class: "dialog ssh-prompt", role: "dialog", "aria-modal": "true", "aria-labelledby": "ssh-prompt-heading",
                    onkeydown: move |event| { if event.key() == Key::Escape { event.stop_propagation(); respond.call(None); } },
                    h2 { id: "ssh-prompt-heading", "SSH Password Required" }
                    p { class: "muted", "T3 needs your SSH password to connect to " code { "{target}" } ". The password is passed to the local SSH process for this connection attempt and is not saved by T3 Code." }
                    div { class: "ssh-prompt-line",
                        strong { "{request.prompt}" }
                        span { class: "muted", role: "timer", if expired { "Expired" } else { {logic::format_remaining(left)} } }
                    }
                    input { r#type: "password", name: "ssh-password", "aria-label": "SSH password", autocomplete: "current-password", "data-request": "{request.request_id}", disabled: responding() || expired, value: "{password}",
                        onmounted: move |event| { let element = event.data(); spawn(async move { let _ = element.set_focus(true).await; }); },
                        oninput: move |event| password.set(event.value()),
                        onkeydown: move |event| { if event.key() == Key::Enter { event.prevent_default(); respond.call(Some(password.peek().clone())); } },
                    }
                    if let Some(message) = shown_error { p { role: "alert", class: "publish-error", "{message}" } }
                    else { p { class: "muted", "Use SSH keys to avoid repeated password prompts on new SSH sessions." } }
                    div { class: "dialog-actions",
                        button { r#type: "button", "aria-label": "Cancel SSH authentication", disabled: responding(), onclick: move |_| respond.call(None), if expired { "Dismiss" } else { "Cancel" } }
                        button { class: "primary", r#type: "button", "aria-label": "Continue", disabled: responding() || expired, onclick: move |_| respond.call(Some(password.peek().clone())), "Continue" }
                    }
                }
            }
        }
    }
}
