//! Keeping a provider current and installed, from the provider settings page: refreshing what the
//! server knows of each provider, updating one with its package manager, installing and removing the
//! managed Codex runtime, adding an instance, and an ACP registry agent's sessions, providers and
//! sign-out. Port of `ProviderSettingsPanel.tsx`, `ProviderInstanceCard.tsx`,
//! `CodexSetupSection.tsx`, `AcpSessionManagementSection.tsx` and `AddProviderInstanceDialog.tsx`.
//!
//! Nothing here polls. A refresh or an update is answered on the configuration stream the page
//! already holds, the install stream is held only while its row is mounted, and every request that
//! changes something on the server's machine disables its control until the server has answered.
//! An answer for another connection, instance or project than the one that asked is dropped.
use crate::{
    notice::{self, Level},
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
    settings_common::{Confirm, connected_allows, copy_text},
};
use dioxus::prelude::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use t3_client::{connection::ConnectionStatus, provider_manage as logic, provider_settings as policy};
use t3_contracts::AuthEnvironmentScope;

/// Releases a control's in-flight flag when its request ends, answered or abandoned.
struct Busy<T: 'static>(Signal<T>, fn() -> T);
impl<T: 'static> Drop for Busy<T> {
    fn drop(&mut self) {
        if let Ok(mut value) = self.0.try_write() {
            *value = (self.1)();
        }
    }
}
fn flag(signal: Signal<bool>) -> Busy<bool> {
    Busy(signal, || false)
}

/// Whether the connection is up and grants `scope` at the moment of asking.
fn allowed_now(state: Store<UiModel>, scope: AuthEnvironmentScope) -> bool {
    let model = state.peek();
    model.status == ConnectionStatus::Connected && model.destination.as_ref().is_some_and(|id| model.environments.allows(id, scope))
}

/// A request whose answer only matters to the connection that sent it. After it, the page reads the
/// configuration again when the live stream is not the one carrying it.
async fn ask(transport: &TransportHandle, state: Store<UiModel>, method: &str, input: Value) -> Option<Result<Value, String>> {
    let owner = runtime::connection_owner(transport, state)?;
    let result = runtime::request_value_reported(transport.clone(), state, method, input).await;
    (runtime::connection_owner(transport, state).as_ref() == Some(&owner)).then_some(result)
}
async fn follow_config(transport: &TransportHandle, state: Store<UiModel>) {
    if !runtime::live_config_active(transport, state) {
        let _ = runtime::request_value_reported(transport.clone(), state, "server.getConfig", json!({})).await;
    }
}

fn driver_label(driver: &str) -> String {
    policy::drivers().iter().find(|d| d["value"] == driver).and_then(|d| d["label"].as_str()).unwrap_or(driver).to_owned()
}

/// A failure, with what was being done in front of what the server said.
fn failed(title: &str, message: &str) -> String {
    format!("{title}: {message}")
}

// ---- the page's toolbar ---------------------------------------------------------------------

/// Refresh and add: the controls of the page rather than of one provider.
#[component]
pub(crate) fn Toolbar(state: Store<UiModel>, transport: TransportHandle, providers: Vec<Value>, instance_ids: Vec<String>, environment: String, allowed: bool) -> Element {
    let mut pending = use_signal(|| false);
    let mut failure = use_signal(|| None::<String>);
    let mut adding = use_signal(|| false);
    let readable = connected_allows(state, AuthEnvironmentScope::OrchestrationRead);
    let checked = logic::checked_label(&providers);
    let refresh = {
        let transport = transport.clone();
        move |_| {
            if *pending.peek() || !allowed_now(state, AuthEnvironmentScope::OrchestrationRead) {
                return;
            }
            pending.set(true);
            failure.set(None);
            let busy = flag(pending);
            let transport = transport.clone();
            spawn(async move {
                let _busy = busy;
                match ask(&transport, state, logic::REFRESH, json!({"refreshModels":true})).await {
                    Some(Ok(_)) => follow_config(&transport, state).await,
                    Some(Err(message)) => failure.set(Some(failed("Could not refresh providers", &message))),
                    None => {}
                }
            });
        }
    };
    rsx! { div { class: "provider-toolbar",
        button { "aria-label": "Refresh provider status", "aria-busy": "{pending}", disabled: *pending.read() || !readable, onclick: refresh,
            if *pending.read() { "Refreshing providers" } else if let Some(checked) = checked { "{checked}" } else { "Refresh providers" }
        }
        if allowed { button { "aria-label": "Add provider", onclick: move |_| adding.set(true), "\u{ff0b} Add provider" } }
        if let Some(message) = failure.read().clone() { p { class: "error-banner", role: "alert", "{message}" } }
        if *adding.read() { AddInstance { state, transport, instance_ids, environment, onclose: move |_| adding.set(false) } }
    } }
}

// ---- adding an instance -------------------------------------------------------------------------

#[component]
fn AddInstance(state: Store<UiModel>, transport: TransportHandle, instance_ids: Vec<String>, environment: String, onclose: EventHandler<()>) -> Element {
    let mut driver = use_signal(|| "codex".to_owned());
    let mut label = use_signal(String::new);
    let mut id_override = use_signal(|| None::<String>);
    let mut configs = use_signal(BTreeMap::<String, Value>::new);
    let mut attempted = use_signal(|| false);
    let mut saving = use_signal(|| false);
    let mut failure = use_signal(|| None::<String>);
    let taken = {
        let ids = instance_ids.clone();
        move |id: &str| ids.iter().any(|existing| existing == id)
    };
    let current = driver.read().clone();
    // A local ACP command is the one ACP source an instance can start from here.
    let config = configs.read().get(&current).cloned().unwrap_or_else(|| if current == "acpRegistry" { json!({"source":"local","commandArgs":[]}) } else { json!({}) });
    let suggestion = logic::available_instance_id(&current, &label.read(), &taken);
    let id = id_override.read().clone().unwrap_or(suggestion.clone());
    let id_error = logic::validate_instance_id(&id, &taken);
    let local_missing = current == "acpRegistry" && config["commandPath"].as_str().is_none_or(|path| path.trim().is_empty());
    let fields = policy::fields(&current, &config);
    let save = {
        let (id, current, ids, config) = (id.clone(), current.clone(), instance_ids.clone(), if current == "codex" { let mut config = config.clone(); config["setupMode"] = json!("existing"); config } else { config.clone() });
        move |_| {
            attempted.set(true);
            if *saving.peek() || !allowed_now(state, AuthEnvironmentScope::ProvidersManage) || logic::validate_instance_id(&id, &|candidate| ids.iter().any(|existing| existing == candidate)).is_some() || local_missing {
                return;
            }
            saving.set(true);
            failure.set(None);
            let busy = flag(saving);
            let input = logic::create_input(&id, &current, &label.peek(), &config);
            let (transport, id, current) = (transport.clone(), id.clone(), current.clone());
            spawn(async move {
                let _busy = busy;
                match ask(&transport, state, "server.updateSettings", input).await {
                    Some(Ok(_)) => {
                        follow_config(&transport, state).await;
                        notice::push(state, Level::Success, "Provider instance added", format!("{} instance '{id}' was added.", driver_label(&current)));
                        onclose.call(());
                    }
                    Some(Err(message)) => failure.set(Some(failed("Could not add provider instance", &message))),
                    None => {}
                }
            });
        }
    };
    rsx! { div { class: "modal-backdrop", onclick: move |_| onclose.call(()),
        div { class: "dialog wide-dialog", role: "dialog", "aria-modal": "true", "aria-label": "Add provider instance", tabindex: "-1",
            onclick: move |event| event.stop_propagation(),
            onmounted: move |event| { let element = event.data(); spawn(async move { let _ = element.set_focus(true).await; }); },
            onkeydown: move |event| { if event.key() == Key::Escape { event.stop_propagation(); onclose.call(()); } },
            h2 { "Add provider" }
            p { class: "muted", "Adds an instance to {environment}." }
            div { class: "provider-field", div { strong { "Provider" } }
                select { "aria-label": "Provider driver", value: "{current}", disabled: *saving.read(), onchange: move |event| { driver.set(event.value()); id_override.set(None); failure.set(None); },
                    for option in policy::drivers().iter() { option { key: "{option[\"value\"]}", value: option["value"].as_str().unwrap_or_default(), selected: option["value"] == current.as_str(), { if option["value"] == "acpRegistry" { "Local ACP command".to_owned() } else { option["label"].as_str().unwrap_or_default().to_owned() } } } }
                }
            }
            div { class: "provider-field", div { strong { "Label" } p { class: "muted", "Shown in the provider list." } }
                input { "aria-label": "Label", placeholder: "e.g. Work", value: "{label}", disabled: *saving.read(), oninput: move |event| label.set(event.value()) }
            }
            div { class: "provider-field", div { strong { "Instance ID" } p { class: "muted", "Used in settings and threads. Cannot be changed later." } }
                input { "aria-label": "Instance ID", placeholder: "{current}_work", value: "{id}", disabled: *saving.read(), oninput: move |event| id_override.set(Some(event.value())) }
            }
            if *attempted.read() || id_override.read().is_some() { if let Some(error) = id_error.clone() { p { class: "error-banner", role: "alert", "{error}" } } }
            for field in fields.iter().filter(|field| !(current == "acpRegistry" && field.key == "source")) {
                crate::provider_settings::ProviderField { key: "{field.key}", field: field.clone(), config: config.clone(), disabled: *saving.read(), onchange: {
                    let (current, config) = (current.clone(), config.clone());
                    move |(key, value): (String, Value)| {
                        let Some(field) = policy::fields(&current, &config).iter().find(|field| field.key == key) else { return };
                        let next = policy::next_field_value(&config, field, &value).unwrap_or_else(|| json!({}));
                        configs.write().insert(current.clone(), next);
                    }
                } }
            }
            if current == "acpRegistry" {
                CommandArguments { config: config.clone(), disabled: *saving.read(), onchange: { let current = current.clone(); move |next: Value| { configs.write().insert(current.clone(), next); } } }
                if *attempted.read() && local_missing { p { class: "error-banner", role: "alert", "Executable is required." } }
            }
            if let Some(message) = failure.read().clone() { p { class: "error-banner", role: "alert", "{message}" } }
            div { class: "dialog-actions",
                button { r#type: "button", "aria-label": "Cancel adding a provider", onclick: move |_| onclose.call(()), "Cancel" }
                button { class: "primary", r#type: "button", "aria-label": "Add instance", disabled: *saving.read(), onclick: save, if *saving.read() { "Adding..." } else { "Add instance" } }
            }
        }
    } }
}

// ---- command arguments ------------------------------------------------------------------------

/// One literal argument per row, in launch order, for a local ACP command.
#[component]
pub(crate) fn CommandArguments(config: Value, disabled: bool, onchange: EventHandler<Value>) -> Element {
    let args = logic::command_arguments(&config);
    let publish = {
        let config = config.clone();
        move |next: Vec<String>| onchange.call(logic::with_command_arguments(&config, &next))
    };
    rsx! { div { class: "provider-field provider-arguments",
        div { strong { "Arguments" } p { class: "muted", "One literal argument per row, in launch order." } }
        div { class: "provider-argument-rows",
            for (index, argument) in args.iter().cloned().enumerate() {
                div { key: "{index}", class: "provider-argument",
                    CommittedInput { label: format!("Argument {}", index + 1), value: argument, disabled, oncommit: { let (args, publish) = (args.clone(), publish.clone()); move |next: String| { let mut args = args.clone(); args[index] = next; publish(args); } } }
                    button { r#type: "button", "aria-label": "Remove argument {index + 1}", disabled, onclick: { let (args, publish) = (args.clone(), publish.clone()); move |_| { let mut args = args.clone(); args.remove(index); publish(args); } }, "\u{d7}" }
                }
            }
            button { r#type: "button", "aria-label": "Add argument", disabled, onclick: { let (args, publish) = (args.clone(), publish.clone()); move |_| { let mut args = args.clone(); args.push(String::new()); publish(args); } }, "\u{ff0b} Add argument" }
        }
    } }
}

/// A text box whose edit is committed when it loses focus, so that one keystroke is not one settings write.
#[component]
fn CommittedInput(label: String, value: String, disabled: bool, oncommit: EventHandler<String>) -> Element {
    let mut draft = use_signal(|| value.clone());
    let incoming = value.clone();
    use_effect(use_reactive((&incoming,), move |(incoming,)| draft.set(incoming)));
    rsx! { input { "aria-label": "{label}", r#type: "text", autocomplete: "off", spellcheck: "false", disabled, value: "{draft}",
        oninput: move |event| draft.set(event.value()),
        onblur: move |_| { if *draft.peek() != value { oncommit.call(draft.peek().clone()); } } } }
}

// ---- one provider: its version, update and managed runtime --------------------------------------

/// What a provider's editor says about keeping it current and usable, and what it offers to do.
#[component]
pub(crate) fn Maintenance(state: Store<UiModel>, transport: TransportHandle, provider: Value, display: String, environment: String, allowed: bool, custom_binary: bool) -> Element {
    let instance = provider["instanceId"].as_str().unwrap_or_default().to_owned();
    let driver = provider["driver"].as_str().unwrap_or_default().to_owned();
    let driver_name = driver_label(&driver);
    let advisory = logic::advisory(&provider);
    let outcome = logic::update_outcome(&provider);
    let version = logic::version_label(provider["version"].as_str());
    let mut updating = use_signal(|| false);
    let mut open = use_signal(|| false);
    let mut asking = use_signal(|| None::<Option<String>>);
    let mut failure = use_signal(|| None::<String>);
    let active = *updating.read() || logic::update_active(&provider);
    // What the version popover offers: the recommended version when compatibility names one,
    // otherwise the update the server can run. Without the right, only the command is shown.
    let action = advisory.as_ref().and_then(|advisory| match &advisory.target_version {
        Some(target) => (allowed && logic::can_install_recommended(&provider)).then(|| Some(target.clone())),
        None => (allowed && logic::is_update_candidate(&provider)).then_some(None),
    });
    let run = use_callback({
        let (transport, instance, driver, driver_name) = (transport.clone(), instance.clone(), driver.clone(), driver_name.clone());
        move |target: Option<String>| {
            if *updating.peek() || !allowed_now(state, AuthEnvironmentScope::ProvidersManage) {
                return;
            }
            asking.set(None);
            updating.set(true);
            failure.set(None);
            let busy = flag(updating);
            let (transport, input, driver_name) = (transport.clone(), logic::update_input(&driver, &instance, target.as_deref()), driver_name.clone());
            spawn(async move {
                let _busy = busy;
                match ask(&transport, state, logic::UPDATE, input).await {
                    Some(Ok(_)) => follow_config(&transport, state).await,
                    Some(Err(message)) => failure.set(Some(failed(&format!("Could not update {driver_name}"), &message))),
                    None => {}
                }
            });
        }
    });
    let copy = {
        let (command, name) = (advisory.as_ref().and_then(|advisory| advisory.update_command.clone()), display.clone());
        move |_| {
            let (command, name) = (command.clone(), name.clone());
            spawn(async move {
                let Some(command) = command else { return };
                if copy_text(&command).await {
                    notice::push(state, Level::Success, format!("{name} update command copied"), "Run it in a terminal when you are ready to update.");
                } else {
                    notice::push(state, Level::Error, format!("Could not copy {name} update command"), "The clipboard is not available.");
                }
            });
        }
    };
    let question = asking.read().clone().map(|target| {
        let command = advisory.as_ref().and_then(|advisory| advisory.update_command.clone());
        let (title, body) = logic::update_question(&display, &environment, command.as_deref().filter(|_| target.is_none()), target.as_deref());
        (target, title, body)
    });
    rsx! {
        div { class: "provider-maintenance",
            div { class: "provider-version",
                if let Some(version) = version.clone() { code { "{version}" } }
                if let Some(advisory) = advisory.clone() {
                    button { r#type: "button", class: if advisory.strong { "provider-advisory strong" } else { "provider-advisory" }, "aria-label": "{advisory.title} \u{2014} view details", "aria-expanded": "{open}", onclick: move |_| { let now = *open.peek(); open.set(!now) },
                        if advisory.compatibility_warning { "\u{26a0} " } else { "\u{2191} " } "{advisory.title}"
                    }
                }
            }
            if *open.read() { if let Some(advisory) = advisory.clone() {
                div { class: "provider-advisory-popover", role: "dialog", "aria-label": "{advisory.title}", tabindex: "-1",
                    onmounted: move |event| { let element = event.data(); spawn(async move { let _ = element.set_focus(true).await; }); },
                    onkeydown: move |event| { if event.key() == Key::Escape { event.stop_propagation(); open.set(false); } },
                    strong { "{advisory.title}" }
                    p { class: if advisory.strong { "" } else { "muted" }, "{advisory.detail}" }
                    if let Some(target) = action.clone() {
                        button { r#type: "button", "aria-label": "Run provider update", disabled: active, onclick: move |_| asking.set(Some(target.clone())),
                            if active { "Updating" } else if let Some(target) = &advisory.target_version { "Install {logic::version_label(Some(target)).unwrap_or_default()}" } else { "Update now" }
                        }
                    }
                    if action.is_some() && advisory.update_command.is_some() { p { class: "muted", "or, update manually using" } }
                    if let Some(command) = advisory.update_command.clone() {
                        div { class: "provider-command", code { "{command}" } button { r#type: "button", "aria-label": "Copy update command", onclick: copy, "Copy" } }
                    }
                }
            } }
            if let Some(message) = failure.read().clone() { p { class: "error-banner", role: "alert", "{message}" } }
            if let Some(outcome) = outcome {
                if outcome.running { p { role: "status", "aria-live": "polite", "{outcome.title}. {outcome.detail.clone().unwrap_or_default()}" } }
                else if outcome.failed {
                    div { class: "error-banner", role: "alert", strong { "{outcome.title}" } if let Some(detail) = outcome.detail.clone() { p { "{detail}" } }
                        if let Some(output) = outcome.output.clone() { details { summary { "Output" } pre { "{output}" } } } }
                } else { p { class: "muted", role: "status", "{outcome.title}. {outcome.detail.clone().unwrap_or_default()}" } }
            }
            if provider["setup"]["canInstall"] == true { Runtime { state, transport: transport.clone(), provider: provider.clone(), display: driver_name.clone(), environment: environment.clone(), custom_binary } }
            if let Some((target, title, body)) = question {
                Confirm { label: "update", title, body, confirm: "Confirm", busy: !allowed, oncancel: move |_| asking.set(None), onconfirm: move |_| run.call(target.clone()) }
            }
        }
    }
}

// ---- the managed runtime ----------------------------------------------------------------------

/// The runtime row of a provider the server downloads and manages. Its stream is held while the row
/// is mounted and the connection can read; download progress repaints this row and nothing around it.
#[component]
fn Runtime(state: Store<UiModel>, transport: TransportHandle, provider: Value, display: String, environment: String, custom_binary: bool) -> Element {
    let instance = provider["instanceId"].as_str().unwrap_or_default().to_owned();
    let _status = state.status().read().clone();
    let owner = runtime::connection_owner(&transport, state);
    let readable = connected_allows(state, AuthEnvironmentScope::OrchestrationRead);
    let manage = connected_allows(state, AuthEnvironmentScope::ProvidersManage);
    let mut install = use_signal(|| None::<Value>);
    let mut query_error = use_signal(|| None::<String>);
    let mut error = use_signal(|| None::<String>);
    let mut pending = use_signal(|| None::<&'static str>);
    let mut removing = use_signal(|| false);
    let stream_transport = transport.clone();
    use_resource(use_reactive((&owner, &readable, &instance), move |(owner, readable, instance)| {
        let transport = stream_transport.clone();
        async move {
            install.set(None);
            query_error.set(None);
            let Some(owner) = owner.filter(|_| readable) else { return };
            let mut stream = match runtime::request_stream(&transport, state, logic::INSTALL_SUBSCRIBE, json!({"instanceId":instance})) {
                Ok(stream) => stream,
                Err(cause) => {
                    query_error.set(Some(cause));
                    return;
                }
            };
            while let Some(chunk) = stream.next().await {
                if runtime::connection_owner(&transport, state).as_ref() != Some(&owner) {
                    return;
                }
                let values = match chunk {
                    Ok(values) => values,
                    Err(cause) => {
                        query_error.set(Some(cause));
                        return;
                    }
                };
                let decoded: Result<Vec<t3_contracts::ProviderInstallState>, _> = values.iter().cloned().map(serde_json::from_value).collect();
                match decoded {
                    Ok(states) => {
                        if let Some(last) = states.last() {
                            install.set(Some(serde_json::to_value(last).expect("validated install state")));
                        }
                    }
                    Err(cause) => {
                        query_error.set(Some(format!("Invalid install state: {cause}")));
                        return;
                    }
                }
            }
        }
    }));
    let view = logic::install_view(install.read().as_ref(), &provider, &display, custom_binary);
    let busy = pending.read().is_some() || !manage || query_error.read().is_some();
    let act = use_callback({
        let transport = transport.clone();
        move |(label, method, input): (&'static str, &'static str, Value)| {
            if pending.peek().is_some() || !allowed_now(state, AuthEnvironmentScope::ProvidersManage) {
                return;
            }
            pending.set(Some(label));
            error.set(None);
            let busy = Busy(pending, || None);
            let transport = transport.clone();
            spawn(async move {
                let _busy = busy;
                let Some(result) = ask(&transport, state, method, input).await else { return };
                match result {
                    // The answer is the state the stream is about to carry; it is applied here so the row
                    // does not wait for it, and the stream's own copy replaces it.
                    Ok(value) if value["driver"].is_string() => install.set(Some(value)),
                    Ok(_) => {}
                    Err(message) => error.set(Some(message)),
                }
            });
        }
    });
    let start = { let instance = instance.clone(); move |_| act.call(("Starting installation", logic::INSTALL_START, json!({"instanceId":instance}))) };
    let cancel = { let (instance, operation) = (instance.clone(), view.operation_id.clone()); move |_| { if let Some(operation) = operation.clone() { act.call(("Cancelling installation", logic::INSTALL_CANCEL, json!({"instanceId":instance,"operationId":operation}))) } } };
    let remove = { let instance = instance.clone(); move |_| { removing.set(false); act.call(("Removing runtime", logic::INSTALL_REMOVE, json!({"instanceId":instance}))) } };
    rsx! { section { class: "provider-runtime", "aria-label": "{display} runtime",
        div { class: "provider-runtime-row",
            div { strong { "Runtime" }
                p { class: "muted", role: "status", "{view.status}" }
                if custom_binary { p { class: "muted", "{logic::KEEPS_CUSTOM_BINARY}" } }
                if provider["installed"] != true && provider["setup"]["canInstall"] != true { p { class: "muted", "{logic::INSTALL_UNAVAILABLE}" } }
                if let Some((done, total)) = view.progress { progress { "aria-label": "{display} download", value: "{done}", max: "{total}" } }
                if let Some(message) = view.message.clone() { p { class: "muted", "{message}" } }
            }
            div { class: "provider-runtime-actions",
                match view.action {
                    logic::InstallAction::Cancel => rsx! { button { r#type: "button", "aria-label": "Cancel installation", disabled: busy, onclick: cancel, "Cancel installation" } },
                    logic::InstallAction::Install => rsx! { button { r#type: "button", "aria-label": "{view.action_label}", disabled: busy || install.read().is_none(), onclick: start, "{view.action_label}" } },
                    logic::InstallAction::None => rsx! {},
                }
                if view.can_remove { button { r#type: "button", "aria-label": "Remove downloaded runtime", title: "Remove downloaded runtime", disabled: busy, onclick: move |_| removing.set(true), "\u{1f5d1}" } }
            }
        }
        if let Some(label) = *pending.read() { p { class: "muted", role: "status", "{label}\u{2026}" } }
        if let Some(message) = error.read().clone().or_else(|| query_error.read().clone()) { p { class: "error-banner", role: "alert", "{message}" } }
        if *removing.read() {
            Confirm { label: "removing the runtime", title: logic::remove_question(&display, &environment), body: "", confirm: "Confirm", busy: !manage, oncancel: move |_| removing.set(false), onconfirm: remove }
        }
    } }
}

// ---- ACP registry agents --------------------------------------------------------------------------

/// What an ACP registry agent offers beyond running sessions: listing and importing the sessions it
/// keeps, deleting them, routing its model providers, and signing out. Each part appears only when
/// the agent's snapshot says it can.
#[component]
pub(crate) fn Acp(state: Store<UiModel>, transport: TransportHandle, provider: Value, allowed: bool) -> Element {
    let abilities = logic::acp_abilities(&provider);
    let instance = provider["instanceId"].as_str().unwrap_or_default().to_owned();
    let projects: Vec<(String, String)> = state.shell().read().snapshot.as_ref().map(|snapshot| snapshot.projects.iter().map(|project| (project.id.to_string(), project.title.to_string())).collect()).unwrap_or_default();
    let readable = connected_allows(state, AuthEnvironmentScope::OrchestrationRead);
    let operate = connected_allows(state, AuthEnvironmentScope::OrchestrationOperate);
    let mut project = use_signal(|| projects.first().map(|(id, _)| id.clone()));
    let mut sessions = use_signal(Vec::<Value>::new);
    let mut cursor = use_signal(|| None::<String>);
    let mut loading = use_signal(|| false);
    let mut importing = use_signal(|| None::<String>);
    let mut deleting = use_signal(|| None::<String>);
    let mut asking = use_signal(|| None::<Question>);
    let mut providers = use_signal(Vec::<Value>::new);
    let mut drafts = use_signal(BTreeMap::<String, logic::ProviderDraft>::new);
    let mut saving = use_signal(|| None::<String>);
    let mut loading_providers = use_signal(|| false);
    let mut logging_out = use_signal(|| false);
    let mut failure = use_signal(|| None::<(String, String)>);
    let chosen = project.read().clone().filter(|chosen| projects.iter().any(|(id, _)| id == chosen)).or_else(|| projects.first().map(|(id, _)| id.clone()));
    let working = *loading.read() || importing.read().is_some() || deleting.read().is_some() || *loading_providers.read() || saving.read().is_some();
    // Runs one request for this instance and project and applies its answer only while both still
    // are. A request that is not sent drops its continuation, which releases the control it held.
    let call = use_callback({
        let (transport, instance, chosen) = (transport.clone(), instance.clone(), chosen.clone());
        move |(method, extra, scope, then): (&'static str, Value, AuthEnvironmentScope, Then)| {
            let Some(chosen) = chosen.clone() else { return };
            if !allowed_now(state, scope) {
                return;
            }
            failure.set(None);
            let (transport, instance) = (transport.clone(), instance.clone());
            let mut input = json!({"instanceId":instance,"projectId":chosen});
            for (key, value) in extra.as_object().into_iter().flatten() {
                input[key] = value.clone();
            }
            spawn(async move {
                if let Some(result) = ask(&transport, state, method, input).await {
                    if *project.peek() == Some(chosen) || method == logic::LOGOUT {
                        then(result);
                    }
                }
            });
        }
    });
    let list = use_callback(move |next: Option<String>| {
        if *loading.peek() {
            return;
        }
        loading.set(true);
        let busy = flag(loading);
        let append = next.is_some();
        let extra = next.map_or(json!({}), |cursor| json!({"cursor":cursor}));
        call.call((logic::LIST_SESSIONS, extra, AuthEnvironmentScope::OrchestrationRead, Box::new(move |result| {
            drop(busy);
            match result {
                Ok(page) => {
                    let merged = logic::merge_sessions(sessions.peek().clone(), &page, append);
                    sessions.set(merged);
                    cursor.set(page["nextCursor"].as_str().map(str::to_owned));
                }
                Err(message) => failure.set(Some(("Could not list ACP sessions".into(), message))),
            }
        })));
    });
    let import = use_callback(move |session: Value| {
        let Some(id) = session["sessionId"].as_str().map(str::to_owned) else { return };
        if importing.peek().is_some() {
            return;
        }
        importing.set(Some(id.clone()));
        let busy = Busy(importing, || None);
        let extra = json!({"sessionId":id,"title":session["title"],"updatedAt":session["updatedAt"]});
        call.call((logic::IMPORT_SESSION, extra, AuthEnvironmentScope::OrchestrationOperate, Box::new(move |result| {
            drop(busy);
            match result {
                Ok(done) => {
                    for row in sessions.write().iter_mut().filter(|row| row["sessionId"] == id.as_str()) {
                        row["importedThreadId"] = done["threadId"].clone();
                    }
                    notice::push(state, Level::Success, if done["imported"] == true { "ACP session imported" } else { "ACP session already imported" }, "");
                }
                Err(message) => failure.set(Some(("Could not import ACP session".into(), message))),
            }
        })));
    });
    let delete = use_callback(move |id: String| {
        if deleting.peek().is_some() {
            return;
        }
        deleting.set(Some(id.clone()));
        let busy = Busy(deleting, || None);
        call.call((logic::DELETE_SESSION, json!({"sessionId":id}), AuthEnvironmentScope::OrchestrationOperate, Box::new(move |result| {
            drop(busy);
            match result {
                Ok(_) => sessions.write().retain(|row| row["sessionId"] != id.as_str()),
                Err(message) => failure.set(Some(("Could not delete ACP session".into(), message))),
            }
        })));
    });
    let list_providers = use_callback(move |()| {
        if *loading_providers.peek() {
            return;
        }
        loading_providers.set(true);
        let busy = flag(loading_providers);
        call.call((logic::LIST_PROVIDERS, json!({}), AuthEnvironmentScope::OrchestrationRead, Box::new(move |result| {
            drop(busy);
            match result {
                Ok(page) => {
                    let rows = page["providers"].as_array().cloned().unwrap_or_default();
                    drafts.set(rows.iter().filter_map(|row| Some((row["providerId"].as_str()?.to_owned(), logic::provider_draft(row)))).collect());
                    providers.set(rows);
                }
                Err(message) => failure.set(Some(("Could not list ACP providers".into(), message))),
            }
        })));
    });
    let save = use_callback({
        let (instance, chosen) = (instance.clone(), chosen.clone());
        move |provider_id: String| {
            if saving.peek().is_some() {
                return;
            }
            let Some(draft) = drafts.peek().get(&provider_id).cloned() else { return };
            if draft.api_type.is_empty() || draft.base_url.is_empty() {
                return;
            }
            let Some(chosen) = chosen.clone() else { return };
            let input = match logic::set_provider_input(&instance, &chosen, &provider_id, &draft) {
                Ok(input) => input,
                Err((title, message)) => {
                    failure.set(Some((title, message)));
                    return;
                }
            };
            saving.set(Some(provider_id));
            let busy = Busy(saving, || None);
            call.call((logic::SET_PROVIDER, input, AuthEnvironmentScope::ProvidersManage, Box::new(move |result| {
                drop(busy);
                match result {
                    Ok(_) => {
                        notice::push(state, Level::Success, "ACP provider configured", "");
                        list_providers.call(());
                    }
                    Err(message) => failure.set(Some(("Could not configure ACP provider".into(), message))),
                }
            })));
        }
    });
    let disable = use_callback(move |provider_id: String| {
        if saving.peek().is_some() {
            return;
        }
        saving.set(Some(provider_id.clone()));
        let busy = Busy(saving, || None);
        call.call((logic::DISABLE_PROVIDER, json!({"providerId":provider_id}), AuthEnvironmentScope::ProvidersManage, Box::new(move |result| {
            drop(busy);
            match result {
                Ok(_) => {
                    notice::push(state, Level::Success, "ACP provider disabled", "");
                    list_providers.call(());
                }
                Err(message) => failure.set(Some(("Could not disable ACP provider".into(), message))),
            }
        })));
    });
    let logout = use_callback(move |()| {
        if *logging_out.peek() {
            return;
        }
        logging_out.set(true);
        let busy = flag(logging_out);
        call.call((logic::LOGOUT, json!({}), AuthEnvironmentScope::ProvidersManage, Box::new(move |result| {
            drop(busy);
            match result {
                Ok(_) => {
                    sessions.set(vec![]);
                    cursor.set(None);
                    notice::push(state, Level::Success, "Logged out of ACP agent", "");
                }
                Err(message) => failure.set(Some(("Could not log out of ACP agent".into(), message))),
            }
        })));
    });
    if !abilities.any() {
        return rsx! {};
    }
    let question = asking.read().clone();
    rsx! { section { class: "provider-acp", "aria-label": "ACP agent management",
        div { class: "provider-acp-head",
            div { strong { "Native sessions" } p { class: "muted", "Resume agent-owned conversations as T3 threads." }
                if abilities.list && projects.is_empty() { p { class: "muted", "Add a project before importing sessions." } } }
            div { class: "provider-acp-controls",
                if abilities.logout { button { r#type: "button", "aria-label": "Log out of ACP agent", disabled: !allowed || *logging_out.read(), onclick: move |_| logout.call(()), if *logging_out.read() { "Logging out" } else { "Log out" } } }
                if (abilities.list || abilities.configure_providers) && projects.len() > 0 {
                    select { "aria-label": "Project for ACP sessions", disabled: !readable || working, value: chosen.clone().unwrap_or_default(),
                        onchange: move |event| { project.set(Some(event.value())); sessions.set(vec![]); cursor.set(None); providers.set(vec![]); drafts.set(BTreeMap::new()); },
                        for (id, title) in projects.iter() { option { key: "{id}", value: "{id}", selected: chosen.as_deref() == Some(id.as_str()), "{title}" } } }
                }
                if abilities.list && projects.len() > 0 {
                    button { r#type: "button", "aria-label": "List ACP sessions", disabled: !readable || *loading.read() || chosen.is_none(), onclick: move |_| list.call(None),
                        if *loading.read() { "Loading" } else if sessions.read().is_empty() { "List sessions" } else { "Refresh" } }
                }
            }
        }
        if let Some((title, message)) = failure.read().clone() { p { class: "error-banner", role: "alert", strong { "{title}" } " {message}" } }
        if abilities.list { if !sessions.read().is_empty() || cursor.read().is_some() {
            div { class: "provider-acp-sessions",
                for session in sessions.read().iter().cloned() {
                    div { key: "{session[\"sessionId\"]}", class: "provider-acp-session",
                        div { span { { session["title"].as_str().or_else(|| session["sessionId"].as_str()).unwrap_or_default().to_owned() } } code { { session["sessionId"].as_str().unwrap_or_default().to_owned() } } }
                        div { class: "provider-acp-session-actions",
                            { let imported = session["importedThreadId"].is_string(); let this = session["sessionId"].as_str().unwrap_or_default().to_owned(); let id = this.clone(); let session_for_import = session.clone(); rsx! {
                                button { r#type: "button", "aria-label": "Import ACP session {this}", disabled: !operate || !abilities.import || imported || importing.read().is_some(), onclick: move |_| import.call(session_for_import.clone()),
                                    if imported { "Imported" } else if importing.read().as_deref() == Some(id.as_str()) { "Importing" } else { "Import" } }
                            } }
                            if abilities.delete { { let imported = session["importedThreadId"].is_string(); let id = session["sessionId"].as_str().unwrap_or_default().to_owned(); let message = logic::delete_session_question(&session); rsx! {
                                button { r#type: "button", "aria-label": "Delete ACP session {id}", disabled: !operate || imported || deleting.read().is_some(), onclick: { let id = id.clone(); move |_| asking.set(Some(Question::DeleteSession { id: id.clone(), message: message.clone() })) },
                                    if deleting.read().as_deref() == Some(id.as_str()) { "Deleting" } else { "Delete" } }
                            } } }
                        }
                    }
                }
                if let Some(next) = cursor.read().clone() {
                    button { r#type: "button", "aria-label": "Load more ACP sessions", disabled: !readable || *loading.read(), onclick: move |_| list.call(Some(next.clone())), if *loading.read() { "Loading" } else { "Load more" } }
                }
            }
        } }
        if abilities.configure_providers {
            div { class: "provider-acp-providers",
                div { class: "provider-acp-head",
                    div { strong { "Agent providers" } p { class: "muted", "Configure non-secret routing. Headers are write-only." } }
                    button { r#type: "button", "aria-label": "List ACP providers", disabled: !readable || *loading_providers.read() || chosen.is_none(), onclick: move |_| list_providers.call(()),
                        if *loading_providers.read() { "Loading" } else if providers.read().is_empty() { "List providers" } else { "Refresh" } }
                }
                for row in providers.read().iter().cloned() {
                    { let provider_id = row["providerId"].as_str().unwrap_or_default().to_owned(); let draft = drafts.read().get(&provider_id).cloned().unwrap_or_else(|| logic::provider_draft(&row)); let required = row["required"] == true; let configured = row["current"].is_object(); let busy = !allowed || saving.read().is_some();
                      let supported: Vec<String> = row["supported"].as_array().map(|list| list.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default();
                      let edit = { let provider_id = provider_id.clone(); move |change: Box<dyn Fn(&mut logic::ProviderDraft)>| { let mut drafts = drafts; if let Some(draft) = drafts.write().get_mut(&provider_id) { change(draft); } } };
                      rsx! { div { key: "{provider_id}", class: "provider-acp-provider",
                        div { class: "provider-acp-head", div { strong { "{provider_id}" } p { class: "muted", { if configured { "Configured" } else { "Disabled" } } { if required { " \u{b7} Required" } else { "" } } } }
                            div { button { r#type: "button", "aria-label": "Save {provider_id}", disabled: busy || draft.api_type.is_empty() || draft.base_url.is_empty(), onclick: { let provider_id = provider_id.clone(); move |_| save.call(provider_id.clone()) }, if saving.read().as_deref() == Some(provider_id.as_str()) { "Saving" } else { "Save" } }
                                if !required && configured { button { r#type: "button", "aria-label": "Disable {provider_id}", disabled: busy, onclick: { let provider_id = provider_id.clone(); move |_| asking.set(Some(Question::DisableProvider { id: provider_id.clone() })) }, "Disable" } } } }
                        select { "aria-label": "{provider_id} protocol", disabled: busy, value: "{draft.api_type}", onchange: { let edit = edit.clone(); move |event| { let value = event.value(); edit(Box::new(move |draft| draft.api_type = value.clone())) } },
                            for protocol in supported { option { key: "{protocol}", value: "{protocol}", selected: protocol == draft.api_type, "{protocol}" } } }
                        input { "aria-label": "{provider_id} base URL", r#type: "url", placeholder: "https://api.example.com", disabled: busy, value: "{draft.base_url}", oninput: { let edit = edit.clone(); move |event| { let value = event.value(); edit(Box::new(move |draft| draft.base_url = value.clone())) } } }
                        input { "aria-label": "{provider_id} write-only headers JSON", r#type: "password", autocomplete: "off", placeholder: "Write-only headers JSON, e.g. {{\"Authorization\":\"Bearer \u{2026}\"}}", disabled: busy, value: "{draft.headers}", oninput: move |event| { let value = event.value(); edit(Box::new(move |draft| draft.headers = value.clone())) } }
                      } } }
                }
            }
        }
        if let Some(question) = question {
            { let message = question.message(); let (title, body) = t3_client::lifecycle::confirm_copy(&message);
              rsx! { Confirm { label: "this ACP change", title, body: body.unwrap_or_default(), confirm: "Confirm", busy: !operate && !allowed, oncancel: move |_| asking.set(None),
                onconfirm: move |_| { asking.set(None); match question.clone() { Question::DeleteSession { id, .. } => delete.call(id), Question::DisableProvider { id } => disable.call(id) } } } } }
        }
    } }
}

/// What runs with a request's answer.
type Then = Box<dyn FnOnce(Result<Value, String>)>;

/// What the ACP section asks before it deletes a session or disables a provider.
#[derive(Debug, Clone, PartialEq)]
enum Question {
    DeleteSession { id: String, message: String },
    DisableProvider { id: String },
}
impl Question {
    fn message(&self) -> String {
        match self {
            Self::DeleteSession { message, .. } => message.clone(),
            Self::DisableProvider { id } => logic::disable_provider_question(id),
        }
    }
}
