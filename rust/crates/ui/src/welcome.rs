//! The first-run welcome wizard (`/welcome`), port of `WelcomeWizard.tsx`, `FirstRunGate.tsx` and `routes/welcome.tsx`:
//! connect a computer, connect its agents, choose projects to import. It overlays the workspace; a fresh install opens it
//! on its own, `/welcome` opens it again, and finishing (or skipping the projects) records `onboardingCompletedAt`.
//!
//! Differences from the original: this app is connected to one computer at a time, so the first step picks the computer
//! this app uses and sets up that one (the original sets up every computer ticked); T3 Connect (the hosted account) has
//! no counterpart here; the privacy policy link is not drawn. Unlike the original, which holds the app back until the gate has decided, the
//! app is already drawn under the gate; its recovery screen (a gate stalled for four seconds, or preferences that could not be read) covers it.
use crate::runtime::{self, TransportHandle, UiModel, UiModelStoreExt};
use dioxus::prelude::*;
use serde_json::json;
use t3_client::{
    connection::ConnectionStatus,
    onboarding::{self, Decision, DecisionInput, Gate as GateState, GateEvent, ProviderState},
};
use t3_contracts::{AuthEnvironmentScope, ServerProvider};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Step {
    #[default]
    Connect,
    Agents,
    Projects,
}
impl Step {
    const ALL: [(Step, &'static str); 3] = [(Step::Connect, "Connect"), (Step::Agents, "Agents"), (Step::Projects, "Projects")];
}

/// The agent whose install or sign-in command the setup terminal holds.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AgentTerminal {
    pub driver: String,
    pub instance_id: String,
    pub cwd: String,
    pub command: String,
    pub terminal_id: String,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Model {
    pub open: bool,
    pub step: Step,
    pub terminal: Option<AgentTerminal>,
    /// The gate's answer so far.
    pub gate: GateState,
    /// Completion was asked for and has not been confirmed (the settings write is local, so it is only ever briefly true).
    pub finishing: bool,
}

/// Terminals are keyed by a free-form thread id and the server checks only the working directory.
pub(crate) const THREAD_ID: &str = "onboarding-agent-setup";
const PRIMARY_DRIVERS: [(&str, &str); 2] = [("codex", "Codex"), ("claudeAgent", "Claude Code")];

pub(crate) fn open(state: Store<UiModel>) {
    let mut welcome = state.welcome();
    let mut model = welcome.write();
    model.open = true;
    model.step = Step::Connect;
    model.terminal = None;
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Records that onboarding is done, so the gate never opens the wizard again.
fn complete(state: Store<UiModel>) {
    if let Some(writer) = try_consume_context::<crate::client_settings::Writer>() {
        writer.patch(state, json!({"onboardingCompletedAt": now()}));
    } else {
        // A harness without the preferences writer keeps the value in memory only.
        state.client_settings().write().onboarding_completed_at = Some(now().into());
    }
}

/// Closes the wizard without recording anything: the address left `/welcome`.
pub(crate) fn dismiss(state: Store<UiModel>) {
    if state.welcome().peek().open {
        close(state);
    }
}

fn close(state: Store<UiModel>) {
    let mut welcome = state.welcome();
    let mut model = welcome.write();
    model.open = false;
    model.terminal = None;
    model.finishing = false;
}

/// Finishing: onboarding is recorded, the wizard goes, and the person starts a thread in the project the import landed on, or on the start form.
fn finish(transport: &TransportHandle, state: Store<UiModel>, landing: Option<String>) {
    if state.welcome().peek().finishing {
        return;
    }
    state.welcome().write().finishing = true;
    complete(state);
    close(state);
    runtime::new_thread(transport, state, landing);
}

/// Decides, from what the app knows of the workspace, whether this is a first run, and opens the wizard when it is. Draws nothing.
#[component]
pub(crate) fn Gate(state: Store<UiModel>) -> Element {
    let writer = try_consume_context::<crate::client_settings::Writer>();
    let stall_writer = writer.clone();
    let decision = use_memo(move || {
        let hydrated = writer.as_ref().is_some_and(|writer| writer.document.read().hydrated());
        let completed = state.client_settings().read().onboarding_completed_at.is_some();
        // Once onboarding is recorded the answer is the app, so the workspace is not read for it on every change of the shell.
        if hydrated && completed {
            return (Decision::App, false);
        }
        let live = *state.status().read() == ConnectionStatus::Connected;
        let shell_field = state.shell();
        let shell = shell_field.read();
        let bootstrapped = shell.synchronized && shell.snapshot.is_some();
        let config_field = state.typed_config();
        let config = config_field.read();
        let destination = state.destination().read().as_ref().map(ToString::to_string);
        let welcome = state.server_welcome().read().clone();
        let (projects, threads): (Vec<_>, Vec<_>) = shell
            .snapshot
            .as_ref()
            .map(|snapshot| {
                (
                    snapshot.projects.iter().map(|project| (project.id.to_string(), project.workspace_root.to_string())).collect::<Vec<_>>(),
                    snapshot.threads.iter().filter(|thread| thread.deleted_at.is_none()).map(|thread| (thread.id.to_string(), thread.project_id.to_string(), thread.latest_run_id.is_some(), thread.latest_user_message_at.is_some(), thread.active_provider_thread_id.is_some())).collect::<Vec<_>>(),
                )
            })
            .unwrap_or_default();
        let environment = destination.as_deref().unwrap_or_default();
        let project_facts: Vec<_> = projects.iter().map(|(id, root)| onboarding::ProjectFact { id, environment_id: environment, workspace_root: root }).collect();
        let thread_facts: Vec<_> = threads.iter().map(|(id, project, run, message, runtime)| onboarding::ThreadFact { id, project_id: project, environment_id: environment, has_run: *run, has_user_message: *message, has_runtime: *runtime }).collect();
        let cwd = config.as_ref().map(|config| config.cwd.to_string());
        onboarding::resolve_decision(DecisionInput {
            enabled: true,
            hydrated,
            completed,
            bootstrapped,
            authoritative: live && bootstrapped,
            workspace_authoritative: live && bootstrapped,
            workspace_provenance_authoritative: onboarding::provenance_authoritative(welcome.as_ref()),
            catalog_ready: true,
            server_config_available: config.is_some(),
            workspace_fresh: onboarding::is_fresh_workspace(destination.as_deref(), cwd.as_deref(), welcome.as_ref(), &project_facts, &thread_facts),
            project_count: projects.len(),
            thread_count: threads.len(),
        })
    });
    // A decision that has not come four seconds after the preferences were read gets the recovery screen (`FIRST_RUN_DECISION_TIMEOUT_MS`).
    let hydrated = use_memo(move || stall_writer.as_ref().is_some_and(|writer| writer.document.read().hydrated()));
    use_effect(move || {
        if !hydrated() {
            return;
        }
        spawn(async move {
            delay(STALL_AFTER_MS).await;
            let gate = state.welcome().peek().gate;
            if gate.decision == Decision::Pending {
                state.welcome().write().gate = onboarding::transition(gate, GateEvent::Timeout);
            }
        });
    });
    use_effect(move || {
        let (next, persist) = decision();
        let gate = state.welcome().peek().gate;
        if gate.decision == Decision::Wizard {
            return;
        }
        if persist && state.client_settings().peek().onboarding_completed_at.is_none() {
            complete(state);
        }
        let gate = onboarding::transition(gate, GateEvent::Evidence(next));
        state.welcome().write().gate = gate;
        if gate.decision == Decision::Wizard && !state.welcome().peek().open {
            open(state);
        }
    });
    rsx! {}
}

/// How long the gate may stay undecided before the person is offered a way out.
const STALL_AFTER_MS: u64 = 4_000;
async fn delay(ms: u64) {
    #[cfg(target_arch = "wasm32")]
    gloo_timers::future::TimeoutFuture::new(ms as u32).await;
    #[cfg(not(target_arch = "wasm32"))]
    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
}

/// What covers the window when the first-run decision cannot be made (`FirstRunRecovery`): the device's preferences could not be
/// read, or the server did not confirm the workspace. Nothing here changes the workspace; the person retries or reloads.
#[component]
pub(crate) fn Recovery(state: Store<UiModel>) -> Element {
    let writer = try_consume_context::<crate::client_settings::Writer>();
    let settings_failed = writer.as_ref().is_some_and(|writer| !writer.document.read().hydrated()) && state.client_settings_error().read().is_some();
    let gate = state.welcome().read().gate;
    let stalled = gate.stalled && gate.decision == Decision::Pending;
    if !settings_failed && !stalled {
        return rsx! {};
    }
    let (title, body, action) = if settings_failed {
        ("Could not read settings", "Your saved settings could not be loaded.", "Retry")
    } else {
        ("Still connecting", "T3 Code could not confirm this workspace.", "Reload")
    };
    rsx! {
        main { class: "first-run-recovery", role: "alert",
            h1 { "{title}" }
            p { class: "muted", "{body}" }
            button { r#type: "button", onclick: move |_| {
                if settings_failed {
                    spawn(crate::client_settings::hydrate(state));
                } else {
                    let _ = document::eval("window.location.reload();");
                }
            }, "{action}" }
        }
    }
}

#[component]
pub(crate) fn Host(state: Store<UiModel>, transport: TransportHandle) -> Element {
    if !state.welcome().read().open {
        return rsx! {};
    }
    rsx! { Wizard { state, transport } }
}

#[component]
fn Wizard(state: Store<UiModel>, transport: TransportHandle) -> Element {
    let step = state.welcome().read().step;
    let index = Step::ALL.iter().position(|(candidate, _)| *candidate == step).unwrap_or(0);
    rsx! {
        div { class: "modal-backdrop welcome-backdrop",
            div { class: "dialog welcome-dialog", role: "dialog", "aria-modal": "true", "aria-label": "Set up T3 Code", tabindex: "-1",
                onmounted: move |event| { let element = event.data(); spawn(async move { let _ = element.set_focus(true).await; }); },
                header { class: "welcome-header",
                    div { class: "brand", role: "img", "aria-label": "T3 Code", crate::Wordmark {} span { "Code" } }
                    h2 { "Set up T3 Code" }
                    ol { class: "welcome-steps", "aria-label": "Setup steps",
                        for (position, (candidate, label)) in Step::ALL.into_iter().enumerate() {
                            li { key: "{label}", "aria-current": if position == index { "step" } else { "false" },
                                button { r#type: "button", disabled: position >= index, onclick: move |_| { let mut welcome = state.welcome(); let mut model = welcome.write(); model.step = candidate; model.terminal = None; }, "{position + 1}. {label}" }
                            }
                        }
                    }
                }
                div { class: "welcome-panel",
                    match step {
                        Step::Connect => rsx! { ConnectStep { state, transport: transport.clone() } },
                        Step::Agents => rsx! { AgentsStep { state, transport: transport.clone() } },
                        Step::Projects => rsx! {
                            crate::import_projects::Step { state, transport: transport.clone(), finish: { let transport = transport.clone(); move |landing: Option<String>| finish(&transport, state, landing) } }
                        },
                    }
                }
            }
        }
    }
}

// ---- step 1: connect ----------------------------------------------------------------------------

#[component]
fn ConnectStep(state: Store<UiModel>, transport: TransportHandle) -> Element {
    let status = state.status().read().clone();
    let connected = status == ConnectionStatus::Connected;
    let current = state.destination().read().clone();
    let records: Vec<(String, String, String)> = state.environments().read().records.values().map(|record| (record.id.to_string(), record.label.clone(), record.address.clone())).collect();
    let mut pairing_open = use_signal(|| records.is_empty());
    let mut link = use_signal(String::new);
    let mut pairing = use_signal(|| false);
    let mut error = use_signal(String::new);
    let pair = {
        let transport = transport.clone();
        move |event: FormEvent| {
            event.prevent_default();
            let text = link.peek().trim().to_owned();
            if text.is_empty() || *pairing.peek() {
                return;
            }
            let (Some(address), Some(credential)) = (t3_client::access::address_without_pairing_token(&text), t3_client::access::pairing_token_from_url(&text)) else {
                error.set("That is not a pairing link. It looks like https://your-server:5230/pair#token=\u{2026}".into());
                return;
            };
            error.set(String::new());
            pairing.set(true);
            let transport = transport.clone();
            spawn(async move {
                runtime::pair_and_connect(transport, state, address, credential).await;
                pairing.set(false);
                if *state.status().peek() == ConnectionStatus::Connected {
                    link.set(String::new());
                    pairing_open.set(false);
                }
            });
        }
    };
    rsx! {
        h1 { "Connect your computers" }
        p { class: "muted", "Choose the computer to set up. We\u{2019}ll set up agents and projects on it." }
        if !records.is_empty() {
            fieldset { class: "welcome-computers",
                legend { class: "sr-only", "Computer to set up" }
                for (id, label, address) in records {
                    {
                        let is_current = current.as_ref().is_some_and(|current| current.as_str() == id);
                        let state_label = if is_current { match &status { ConnectionStatus::Connected => "Connected".to_owned(), ConnectionStatus::Blocked(_) | ConnectionStatus::Interrupted(_) => "Not connected".to_owned(), _ => "Connecting\u{2026}".to_owned() } } else { "Saved".to_owned() };
                        rsx! {
                            label { key: "{id}", class: "welcome-computer",
                                input { r#type: "radio", name: "welcome-computer", checked: is_current, disabled: !is_current, "aria-label": "{label}" }
                                span { class: "welcome-computer-text", strong { "{label}" } span { class: "muted", "{address}" } }
                                span { class: "muted", "{state_label}" }
                            }
                        }
                    }
                }
            }
        }
        div { class: "welcome-pairing",
            button { r#type: "button", class: "welcome-disclosure", "aria-expanded": "{pairing_open()}", disabled: pairing(), onclick: move |_| pairing_open.toggle(), if pairing_open() { "\u{25be} Add a computer" } else { "\u{25b8} Add a computer" } }
            if pairing_open() {
                form { onsubmit: pair,
                    label { r#for: "onboarding-pairing-url", "Pairing link" }
                    input { id: "onboarding-pairing-url", r#type: "text", autocomplete: "off", spellcheck: false, placeholder: "https://your-server:5230/pair#token=\u{2026}", value: "{link}", readonly: pairing(), oninput: move |event| link.set(event.value()) }
                    if !error.read().is_empty() { p { class: "error-banner", role: "alert", id: "onboarding-pairing-error", "{error}" } }
                    p { class: "muted", "Run this on the computer with your code." }
                    Command { text: "npx t3 pair" }
                    p { class: "muted", "Start T3 Code first, or run npx t3 serve. Add --tailscale to use your tailnet." }
                    button { class: "primary", r#type: "submit", disabled: pairing() || link.read().trim().is_empty(), if pairing() { "Pairing..." } else { "Pair" } }
                }
            }
        }
        div { class: "dialog-actions",
            button { class: "primary", r#type: "button", "aria-label": "Continue to agents", disabled: !connected || pairing(), onclick: move |_| { let mut welcome = state.welcome(); let mut model = welcome.write(); model.step = Step::Agents; model.terminal = None; }, "Continue \u{2192}" }
        }
    }
}

// ---- step 2: agents -----------------------------------------------------------------------------

fn card_text(state: ProviderState, provider: Option<&ServerProvider>) -> String {
    match state {
        ProviderState::Ready => "Ready to code.".into(),
        ProviderState::Checking => "Checking\u{2026}".into(),
        ProviderState::Disabled => "Disabled".into(),
        ProviderState::Install => "Not installed.".into(),
        ProviderState::SignIn => "Not signed in.".into(),
        ProviderState::Attention => provider.and_then(|provider| provider.message.clone().flatten()).map(|message| message.to_string()).unwrap_or_else(|| "Needs attention.".into()),
    }
}

#[component]
fn AgentsStep(state: Store<UiModel>, transport: TransportHandle) -> Element {
    // Re-probe on entry so a freshly installed CLI shows up without a manual refresh.
    use_hook({
        let transport = transport.clone();
        move || refresh(&transport, state)
    });
    let providers: Vec<ServerProvider> = state.typed_config().read().as_ref().map(|config| config.providers.0.clone()).unwrap_or_default();
    let by_driver = onboarding::by_driver(&providers);
    let machine = state.destination().read().as_ref().and_then(|id| state.environments().read().records.get(id).map(|record| record.label.clone())).unwrap_or_else(|| "Computer".into());
    let operate = *state.status().read() == ConnectionStatus::Connected && state.destination().read().as_ref().is_some_and(|id| state.environments().read().allows(id, AuthEnvironmentScope::TerminalOperate));
    let settings = state.config().read()["settings"].clone();
    let os = state.config().read()["environment"]["platform"]["os"].as_str().unwrap_or("linux").to_owned();
    let cwd = state.typed_config().read().as_ref().map(|config| config.cwd.to_string()).unwrap_or_default();
    let terminal = state.welcome().read().terminal.clone();
    rsx! {
        h1 { "Connect your agents" }
        p { class: "muted", "Choose an agent to start coding. You can add more later." }
        h2 { class: "welcome-machine", "{machine}" }
        if !operate { p { class: "muted", "This connection cannot control terminals." } }
        div { class: "welcome-agents",
            for (driver, fallback_name) in PRIMARY_DRIVERS {
                {
                    let provider = by_driver.get(driver).copied();
                    let provider_state = onboarding::provider_state(provider);
                    let name = provider.and_then(|provider| provider.display_name.clone().flatten()).map(|name| name.to_string()).unwrap_or_else(|| fallback_name.to_owned());
                    let text = card_text(provider_state, provider);
                    let open_terminal = provider.map(|provider| AgentTerminal {
                        driver: driver.to_owned(),
                        instance_id: provider.instance_id.to_string(),
                        cwd: cwd.clone(),
                        command: if provider.installed { onboarding::login_command(provider, &settings, &os) } else { onboarding::install_command(driver, &os).to_owned() },
                        terminal_id: format!("onboarding-{driver}-{}", uuid::Uuid::new_v4()),
                    });
                    let action = if provider_state == ProviderState::SignIn { "Sign in to" } else { "Install" };
                    let busy = terminal.as_ref().is_some_and(|open| open.driver == driver);
                    rsx! {
                        div { key: "{driver}", class: "welcome-agent", "data-agent": "{driver}", "data-agent-state": "{provider_state:?}",
                            div { class: "welcome-agent-text", strong { "{name}" } span { class: "muted", "{text}" } }
                            match provider_state {
                                ProviderState::Ready => rsx! { span { class: "welcome-ready", "\u{2713} Ready" } },
                                ProviderState::Install | ProviderState::SignIn => rsx! {
                                    button { r#type: "button", "aria-label": "{action} {name}", disabled: busy || !operate || open_terminal.is_none(),
                                        onclick: move |_| { if let Some(session) = open_terminal.clone() { state.welcome().write().terminal = Some(session); } },
                                        if provider_state == ProviderState::SignIn { "Sign in" } else { "Install" } }
                                },
                                _ => rsx! {},
                            }
                        }
                    }
                }
            }
        }
        if let Some(session) = terminal {
            SetupTerminal { key: "{session.terminal_id}", state, transport: transport.clone(), session }
        }
        div { class: "dialog-actions",
            button { class: "primary", r#type: "button", "aria-label": "Continue to projects", onclick: move |_| { let mut welcome = state.welcome(); let mut model = welcome.write(); model.step = Step::Projects; model.terminal = None; }, "Continue \u{2192}" }
        }
    }
}

fn refresh(transport: &TransportHandle, state: Store<UiModel>) {
    let transport = transport.clone();
    spawn(async move {
        if crate::provider_manage::ask(&transport, state, t3_client::provider_manage::REFRESH, json!({})).await.is_some() {
            crate::provider_manage::follow_config(&transport, state).await;
        }
    });
}

/// The terminal that holds the install or sign-in command. The command is typed but not submitted, so the person reviews it and presses Enter.
#[component]
fn SetupTerminal(state: Store<UiModel>, transport: TransportHandle, session: AgentTerminal) -> Element {
    let mut phase = use_signal(|| None::<Result<(), String>>);
    let environment = state.destination().read().as_ref().map(ToString::to_string).unwrap_or_default();
    let start = {
        let (transport, session) = (transport.clone(), session.clone());
        use_callback(move |_: ()| {
            phase.set(None);
            let (transport, session) = (transport.clone(), session.clone());
            spawn(async move {
                let opened = runtime::request_value_reported(transport.clone(), state, "terminal.open", json!({"threadId":THREAD_ID,"terminalId":session.terminal_id,"cwd":session.cwd,"providerInstanceId":session.instance_id})).await;
                if let Err(message) = opened {
                    phase.set(Some(Err(message)));
                    return;
                }
                let wrote = runtime::request_value_reported(transport.clone(), state, "terminal.write", json!({"threadId":THREAD_ID,"terminalId":session.terminal_id,"data":session.command})).await;
                phase.set(Some(wrote.map(|_| ())));
            });
        })
    };
    use_hook({
        let start = start.clone();
        move || start.call(())
    });
    // Closing the terminal ends its process and deletes its history.
    use_drop({
        let (transport, session) = (transport.clone(), session.clone());
        move || {
            let (transport, session) = (transport.clone(), session.clone());
            dioxus::dioxus_core::spawn_forever(async move {
                let _ = runtime::request_value_reported(transport, state, "terminal.close", json!({"threadId":THREAD_ID,"terminalId":session.terminal_id,"deleteHistory":true})).await;
            });
        }
    });
    let ready = matches!(&*phase.read(), Some(Ok(())));
    let failed = matches!(&*phase.read(), Some(Err(_)));
    let close_transport = transport.clone();
    rsx! {
        div { class: "welcome-terminal", "data-thread-terminal-drawer": "true",
            div { class: "welcome-terminal-bar",
                span { class: "muted", role: "status",
                    if failed { "Could not open the setup terminal." }
                    else if ready { "Review the command, then press Enter to run it." }
                    else { "Preparing command..." }
                }
                if failed { button { r#type: "button", onclick: move |_| start.call(()), "Retry" } }
                button { r#type: "button", "aria-label": "Close terminal", onclick: move |_| { state.welcome().write().terminal = None; refresh(&close_transport, state); }, "Close" }
            }
            if ready {
                div { class: "welcome-terminal-surface",
                    crate::terminal_pane::SetupPane { state, transport, environment, thread_id: THREAD_ID.to_owned(), terminal_id: session.terminal_id.clone(), cwd: session.cwd.clone() }
                }
            }
        }
    }
}

/// A command to run on the other computer, with a button that copies it.
#[component]
fn Command(text: &'static str) -> Element {
    rsx! {
        div { class: "welcome-command",
            code { "{text}" }
            button { r#type: "button", "aria-label": "Copy {text}", onclick: move |_| { let _ = dioxus::document::eval(&format!("navigator.clipboard?.writeText({})", serde_json::to_string(text).unwrap_or_default())); }, "Copy" }
        }
    }
}
