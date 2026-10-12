//! The Integrations settings page (`/settings/integrations`, `IntegrationsSettings.tsx`, `DeviceHostsSettings.tsx`, `DeviceHostEditor.tsx`):
//! whether agents may drive the environment's browser, the device hub and agent device access, what each platform can do, the tools'
//! versions, and the SSH hosts that have simulators and emulators (`device.configure`, `device.list`, `device.testHost`). The
//! environment shown is the one the window is on; the original applies a change to every environment the page is scoped to.
//!
//! The browser defaults (zoom, appearance, link target, profiles) are `browser_settings.rs`. Not ported: importing cookies from another
//! browser, the recording options (nothing records), and the floating preview.
use crate::{
    device_panel,
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt, View},
    settings_common::{Tabs, connected_allows},
};
use dioxus::prelude::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use t3_client::device_panel::{self as logic, Check};
use t3_contracts::{AuthEnvironmentScope, DeviceHostStatus, DevicePlatform, DeviceServiceState, Present};

#[component]
pub(crate) fn Page(state: Store<UiModel>, transport: TransportHandle) -> Element {
    let environment = state.destination().read().as_ref().map(ToString::to_string).unwrap_or_default();
    device_panel::use_follow(state, transport.clone(), environment.clone());
    let snapshot = use_memo({
        let environment = environment.clone();
        move || device_panel::current(state, &environment)
    });
    let (dev, loaded) = snapshot();
    let settings_write = use_memo(move || connected_allows(state, AuthEnvironmentScope::SettingsWrite));
    let config = state.config().read().clone();
    let settings = config["settings"].clone();
    let ready = config.is_object() && settings.is_object();
    let mut pending = use_signal(|| None::<&'static str>);
    let mut failure = use_signal(|| None::<String>);
    let busy = matches!(dev.host_status, DeviceHostStatus::Installing | DeviceHostStatus::Starting);
    let enabled = settings["enableDeviceSupport"].as_bool().unwrap_or(false);
    let agent_devices = settings["enableAgentDeviceAccess"].as_bool().unwrap_or(false);
    let agent_browser = settings["enableAgentBrowserAccess"].as_bool().unwrap_or(false);
    let any_local_platform = dev.hosts.iter().any(|host| host.kind == t3_contracts::DeviceHostKind::Local && host.platforms.iter().any(|platform| platform.available));
    let platforms_revealed = enabled && (dev.host_status == DeviceHostStatus::Ready || !any_local_platform && !dev.hosts.is_empty());
    let local_tools = dev.hosts.iter().find(|host| host.kind == t3_contracts::DeviceHostKind::Local).and_then(|host| host.tools.present().cloned());

    // A change to the device settings is one `device.configure`; turning the hub off turns agent access off with it.
    let configure = {
        let transport = transport.clone();
        use_callback(move |(kind, input): (&'static str, Value)| {
            if !*settings_write.peek() {
                return;
            }
            let transport = transport.clone();
            pending.set(Some(kind));
            failure.set(None);
            spawn(async move {
                let mut input = input;
                if input["enabled"] == true {
                    input["onboardingCompleted"] = json!(true);
                }
                if let Err(message) = runtime::request_value_reported(transport, state, "device.configure", input).await {
                    failure.set(Some(format!("Could not save the device settings. {message}")));
                }
                pending.set(None);
            });
        })
    };
    let list = {
        let transport = transport.clone();
        use_callback(move |(kind, input): (&'static str, Value)| {
            let transport = transport.clone();
            pending.set(Some(kind));
            failure.set(None);
            spawn(async move {
                if let Err(message) = device_panel::list(transport, state, input).await {
                    failure.set(Some(if kind.starts_with("update") { "Update failed. Check this host's network connection and try again.".to_owned() } else { message }));
                }
                pending.set(None);
            });
        })
    };
    let write_setting = {
        let transport = transport.clone();
        use_callback(move |patch: Value| {
            if !*settings_write.peek() {
                return;
            }
            let transport = transport.clone();
            failure.set(None);
            spawn(async move {
                if let Err(message) = runtime::request_value_reported(transport, state, "server.updateSettings", json!({"patch": patch})).await {
                    failure.set(Some(format!("Could not save settings. {message}")));
                }
            });
        })
    };
    let hosts: Vec<Value> = settings["deviceHosts"].as_array().cloned().unwrap_or_default();
    let writable = settings_write() && ready;
    rsx! {
        section { class: "settings-page", Tabs { state, current: View::Integrations }
            div { class: "page-heading", h1 { "Integrations" } }
            p { class: "muted", "What agents can drive in this environment, and the machines with simulators and emulators." }
            if !settings_write() { p { class: "muted", role: "status", "This connection can view these settings but cannot change them." } }
            if let Some(message) = failure.read().clone() { p { class: "error-banner", role: "alert", "{message}" } }
            if !ready {
                p { class: "muted", role: "status", "Connect an environment to choose its settings." }
            } else {
                section { class: "sc-section", id: "integrations-browser", "aria-label": "Browser",
                    h2 { "Browser" }
                    div { class: "setting-row", "data-setting": "enableAgentBrowserAccess",
                        div { class: "font-description", strong { "Agent browser access" } p { class: "muted", "Let agents open and drive the environment's browser in this environment's threads. Projects can override this." } }
                        div { class: "font-controls", input { r#type: "checkbox", role: "switch", "aria-label": "Agent browser access", checked: agent_browser, disabled: !writable,
                            onchange: move |event| write_setting.call(json!({"enableAgentBrowserAccess": event.checked()})) } }
                    }
                    ViewportRow { state }
                    crate::browser_settings::Rows { state, transport: transport.clone() }
                }
                section { class: "sc-section", id: "devices", "aria-label": "Devices",
                    h2 { "Devices" }
                    div { class: "setting-row", "data-setting": "enableDeviceSupport",
                        div { class: "font-description", strong { "Device hub" } p { class: "muted", "{logic::DEVICE_HUB_DESCRIPTION}" }
                            ToolVersions { dev: dev.clone(), tools: local_tools.clone(), kind: "hub" }
                        }
                        div { class: "font-controls",
                            if pending() == Some("hub") || (busy && pending() != Some("agent")) { span { class: "muted", role: "status", "{logic::hub_progress(dev.host_status, true)}" } }
                            input { r#type: "checkbox", role: "switch", "aria-label": "Device hub", checked: enabled, disabled: !writable || !loaded || busy || pending().is_some(),
                                onchange: move |event| { let on = event.checked(); configure.call(("hub", if on { json!({"enabled": true}) } else { json!({"enabled": false, "agentAccessEnabled": false}) })); } } }
                    }
                    if platforms_revealed {
                        div { class: "setting-row", "data-setting": "devicePlatformSupport",
                            div { class: "font-description", strong { "Simulator support" }
                                div { class: "device-platforms",
                                    for platform in [DevicePlatform::Ios, DevicePlatform::Android] {
                                        {
                                            let (ready, message) = logic::platform_setup_status(&dev, platform);
                                            let message = if ready { "Ready".to_owned() } else { message };
                                            rsx! { div { key: "{logic::platform_wire(platform)}", class: if ready { "device-platform ready" } else { "device-platform" }, strong { "{logic::platform_name(platform)}" } p { class: "muted", "{message}" } } }
                                        }
                                    }
                                }
                            }
                            div { class: "font-controls", button { r#type: "button", "aria-label": "Refresh simulator support", disabled: !enabled || busy || pending().is_some(), onclick: move |_| list.call(("check", json!({}))), if pending() == Some("check") { "Checking\u{2026}" } else { "Refresh" } } }
                        }
                    }
                    div { class: "setting-row", "data-setting": "enableAgentDeviceAccess",
                        div { class: "font-description", strong { "Agent device access" } p { class: "muted", "{logic::AGENT_DEVICE_DESCRIPTION}" }
                            ToolVersions { dev: dev.clone(), tools: local_tools.clone(), kind: "agent" }
                        }
                        div { class: "font-controls",
                            if pending() == Some("agent") { span { class: "muted", role: "status", "{logic::agent_progress(dev.host_status, true)}" } }
                            input { r#type: "checkbox", role: "switch", "aria-label": "Agent device access", checked: agent_devices, disabled: !writable || !enabled || !loaded || busy || pending().is_some(),
                                onchange: move |event| configure.call(("agent", json!({"agentAccessEnabled": event.checked()}))) } }
                    }
                    if dev.supports_tool_inspection.present() == Some(&true) || dev.supports_tool_update.present() == Some(&true) {
                        div { class: "device-tool-actions",
                            if dev.supports_tool_update.present() == Some(&true) {
                                for (tool, version) in [("hub", local_tools.as_ref().map(|tools| &tools.hub)), ("agent", local_tools.as_ref().map(|tools| &tools.agent))] {
                                    if let Some(version) = version.filter(|version| !version.installed_versions.contains(&version.required_version)) {
                                        button { key: "{tool}", r#type: "button", "aria-label": "Update {tool} tool", disabled: pending().is_some() || busy, onclick: move |_| list.call((if tool == "hub" { "update-hub" } else { "update-agent" }, json!({"updateTool": tool}))),
                                            if pending() == Some(if tool == "hub" { "update-hub" } else { "update-agent" }) { "Updating\u{2026}" } else { "Update to v{version.required_version}" } }
                                    }
                                }
                            }
                            if dev.supports_tool_inspection.present() == Some(&true) {
                                button { r#type: "button", "aria-label": "Check versions", disabled: pending().is_some() || busy || !loaded, onclick: move |_| list.call(("check", json!({"inspectOnly": true}))), if pending() == Some("check") { "Checking\u{2026}" } else { "Check versions" } }
                            }
                        }
                    }
                    device_panel::HostUpdates { state, transport: transport.clone(), dev: DeviceServiceState { hosts: dev.hosts.iter().filter(|host| host.kind == t3_contracts::DeviceHostKind::Local).cloned().collect(), ..dev.clone() } }
                    DeviceHosts { state, transport: transport.clone(), dev: dev.clone(), hosts, writable, write_setting }
                }
            }
        }
    }
}

/// The versions a host's device tools run at, and what is required (`DeviceToolVersions`).
#[component]
fn ToolVersions(dev: DeviceServiceState, tools: Option<t3_contracts::DeviceToolVersions>, kind: &'static str) -> Element {
    let _ = &dev;
    let label = if kind == "hub" { "Device hub" } else { "Agent device" };
    let selected = tools.as_ref().map(|tools| if kind == "hub" { &tools.hub } else { &tools.agent });
    let text = match selected {
        Some(tool) => match tool.running_version.clone().or_else(|| tool.installed_versions.contains(&tool.required_version).then(|| tool.required_version.clone())).or_else(|| tool.installed_versions.last().cloned()) {
            Some(version) => format!("v{version}"),
            None => "Not installed".to_owned(),
        },
        None => "Version unknown".to_owned(),
    };
    rsx! { p { class: "muted device-versions", title: "Tools update automatically on this host when needed.", "{label}: {text}" } }
}

/// The size a new browser tab opens at (`browserDefaultViewport`): the panel, or a fixed size.
#[component]
fn ViewportRow(state: Store<UiModel>) -> Element {
    let Some(writer) = try_consume_context::<crate::client_settings::Writer>() else { return rsx! {} };
    let viewport = state.client_settings().read().browser_default_viewport.clone();
    let (value, width, height) = match &viewport {
        t3_contracts::PreviewViewportSetting::Fill => ("fill".to_owned(), 1024, 768),
        t3_contracts::PreviewViewportSetting::Freeform { width, height } => ("freeform".to_owned(), width.0, height.0),
        t3_contracts::PreviewViewportSetting::Preset { width, height, .. } => ("freeform".to_owned(), width.0, height.0),
    };
    let write = {
        let writer = writer.clone();
        move |viewport: Value| writer.patch(state, json!({"browserDefaultViewport": viewport}))
    };
    let change = write.clone();
    rsx! {
        div { class: "setting-row", "data-setting": "browserDefaultViewport",
            div { class: "font-description", strong { "Default browser viewport" } p { class: "muted", "Tab size for you and agents. Fill fits the panel; other sizes show the device toolbar." } }
            div { class: "font-controls",
                select { "aria-label": "Default browser viewport", value: "{value}",
                    onchange: move |event| change(if event.value() == "fill" { json!({"_tag": "fill"}) } else { json!({"_tag": "freeform", "width": width, "height": height}) }),
                    option { value: "fill", selected: value == "fill", "Fill panel" }
                    option { value: "freeform", selected: value != "fill", "Responsive" }
                }
                if value != "fill" {
                    input { r#type: "number", min: "240", max: "3840", "aria-label": "Default viewport width", value: "{width}", onchange: { let write = write.clone(); move |event| { if let Ok(next) = event.value().parse::<i64>() { write(json!({"_tag": "freeform", "width": next.clamp(240, 3840), "height": height})); } } } }
                    " \u{d7} "
                    input { r#type: "number", min: "240", max: "3840", "aria-label": "Default viewport height", value: "{height}", onchange: move |event| { if let Ok(next) = event.value().parse::<i64>() { write(json!({"_tag": "freeform", "width": width, "height": next.clamp(240, 3840)})); } } }
                }
            }
        }
    }
}

// ---- the hosts -----------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
struct Draft {
    /// The host before the edit (None for a new one).
    original: Option<Value>,
    id: String,
    label: String,
    target: String,
    identity_file: String,
    port: String,
}

#[component]
fn DeviceHosts(state: Store<UiModel>, transport: TransportHandle, dev: DeviceServiceState, hosts: Vec<Value>, writable: bool, write_setting: Callback<Value>) -> Element {
    let mut editing = use_signal(|| None::<Draft>);
    let mut checks = use_signal(BTreeMap::<String, Check>::new);
    let mut retrying = use_signal(|| None::<String>);
    let mut notice_failure = use_signal(|| None::<String>);
    let test = {
        let transport = transport.clone();
        use_callback(move |host: Value| {
            let key = logic::connection_key(&host);
            if matches!(checks.peek().get(&key), Some(Check::Pending)) {
                return;
            }
            checks.with_mut(|checks| { checks.insert(key.clone(), Check::Pending); });
            let transport = transport.clone();
            spawn(async move {
                let result = match runtime::request_value_reported(transport, state, "device.testHost", host).await {
                    Ok(summary) => logic::check_of(&summary),
                    Err(message) => Check::Failed(message),
                };
                checks.with_mut(|checks| { checks.insert(key, result); });
            });
        })
    };
    let save = {
        let hosts = hosts.clone();
        use_callback(move |(host, remove, original): (Value, bool, Value)| match logic::update_device_hosts(&hosts, &host, remove, &original) {
            Ok(next) => {
                notice_failure.set(None);
                write_setting.call(json!({"deviceHosts": next}));
                editing.set(None);
            }
            Err(message) => notice_failure.set(Some(message)),
        })
    };
    rsx! {
        div { class: "setting-row", "data-setting": "deviceHosts",
            div { class: "font-description", strong { "Device hosts" } p { class: "muted", "Add remote machines with simulator or emulator runtimes installed, and this environment will connect over SSH and set up device tools automatically." } }
            div { class: "font-controls", button { r#type: "button", "aria-label": "Add device host", disabled: !writable || editing.read().is_some(),
                onclick: move |_| editing.set(Some(Draft { original: None, id: uuid::Uuid::new_v4().to_string().replace('-', "_"), label: String::new(), target: String::new(), identity_file: String::new(), port: String::new() })), "Add host" } }
        }
        if let Some(message) = notice_failure.read().clone() { p { class: "error-banner", role: "alert", "{message}" } }
        div { class: "device-host-list", role: "list", "aria-label": "Device hosts",
            if hosts.is_empty() { p { class: "muted", "No device hosts." } }
            for host in hosts.iter().cloned() {
                {
                    let id = host["id"].as_str().unwrap_or_default().to_owned();
                    let label = host["label"].as_str().unwrap_or_default().to_owned();
                    let target = host["target"].as_str().unwrap_or_default().to_owned();
                    let status = dev.host_statuses.iter().find(|(key, _)| key.as_str() == id).map(|(_, status)| status.clone());
                    let summary = dev.hosts.iter().find(|summary| summary.id.as_str() == id).cloned();
                    let check = checks.read().get(&logic::connection_key(&host)).cloned();
                    let progress = logic::host_progress(status.as_ref(), matches!(check, Some(Check::Pending)));
                    let error = match &check {
                        Some(Check::Failed(error)) => Some(error.clone()),
                        Some(Check::Local) => None,
                        _ => logic::host_error(status.as_ref()),
                    };
                    let platforms: Vec<(DevicePlatform, bool)> = match &check {
                        Some(Check::Connected { platforms }) => platforms.clone(),
                        _ => summary.as_ref().map(|summary| summary.platforms.iter().map(|platform| (platform.platform, platform.available)).collect()).unwrap_or_default(),
                    };
                    let retry = logic::can_retry(&dev, status.as_ref());
                    let draft = Draft { original: Some(host.clone()), id: id.clone(), label: label.clone(), target: target.clone(), identity_file: host["identityFile"].as_str().unwrap_or_default().to_owned(), port: host["port"].as_u64().map(|port| port.to_string()).unwrap_or_default() };
                    let (remove_host, test_host, retry_id) = (host.clone(), host.clone(), id.clone());
                    let transport = transport.clone();
                    rsx! {
                        div { key: "{id}", class: "device-host-row", role: "listitem", "data-host": "{id}",
                            div { class: "device-host-main",
                                strong { "{label}" }
                                for (platform, available) in platforms.iter().copied().filter(|(_, available)| *available) { span { key: "{logic::platform_wire(platform)}", class: "muted", role: "img", "aria-label": "{logic::platform_name(platform)} available", "data-available": "{available}", " {logic::platform_name(platform)}" } }
                                p { class: "muted", "{target}" }
                                if matches!(check, Some(Check::Local)) { p { class: "muted", "Already available locally" } }
                                if let Some(error) = error { details { class: "device-host-error", summary { "Connection failed" } p { "{error}" } } }
                            }
                            if let Some(progress) = progress { span { class: "muted", role: "status", "{progress}" } }
                            button { r#type: "button", "aria-label": "Edit {label}", disabled: !writable, onclick: move |_| editing.set(Some(draft.clone())), "Edit" }
                            button { r#type: "button", "aria-label": "Remove {label}", disabled: !writable, onclick: move |_| save.call((remove_host.clone(), true, remove_host.clone())), "Remove" }
                            if retry {
                                button { r#type: "button", "aria-label": "Retry {label}", disabled: !writable || retrying.read().is_some(), onclick: move |_| {
                                    let (transport, id) = (transport.clone(), retry_id.clone());
                                    retrying.set(Some(id.clone()));
                                    spawn(async move { let _ = device_panel::list(transport, state, json!({"retryHostId": id})).await; retrying.set(None); });
                                }, if retrying.read().as_deref() == Some(id.as_str()) { "Retrying\u{2026}" } else { "Retry" } }
                            } else {
                                button { r#type: "button", "aria-label": "Test connection to {label}", disabled: !writable || progress.is_some(), onclick: move |_| test.call(test_host.clone()), "Test connection" }
                            }
                        }
                    }
                }
            }
        }
        if let Some(draft) = editing.read().clone() {
            HostEditor { draft, checks, test, save, on_close: move |_| editing.set(None) }
        }
    }
}

fn availability(available: bool) -> &'static str {
    if available { "available" } else { "unavailable" }
}

/// `DeviceHostEditor`: the form of one host, with a connection test before it is saved.
#[component]
fn HostEditor(draft: Draft, checks: Signal<BTreeMap<String, Check>>, test: Callback<Value>, save: Callback<(Value, bool, Value)>, on_close: EventHandler<()>) -> Element {
    let mut form = use_signal(|| draft.clone());
    let current = form.read().clone();
    let port = current.port.trim().parse::<u64>().ok();
    let host = logic::parse_host_draft(&current.id, if current.label.trim().is_empty() { current.target.trim() } else { current.label.trim() }, current.target.trim(), if current.port.trim().is_empty() { None } else { port }, &current.identity_file);
    let valid = host.is_some() && (current.port.trim().is_empty() || port.is_some());
    let result = host.as_ref().and_then(|host| checks.read().get(&logic::connection_key(host)).cloned());
    let checking = matches!(result, Some(Check::Pending));
    let original = draft.original.clone();
    let title = if draft.original.is_none() { "Add device host" } else { "Edit device host" };
    rsx! {
        div { class: "modal-backdrop", onclick: move |_| on_close.call(()),
            div { class: "dialog", role: "dialog", "aria-modal": "true", "aria-label": "{title}", onclick: move |event| event.stop_propagation(),
                onkeydown: move |event| { if event.key() == Key::Escape { event.stop_propagation(); on_close.call(()); } },
                h2 { "{title}" }
                p { class: "muted", "Connect from this environment. Hosts on the same machine are skipped." }
                label { "Name" input { "aria-label": "Host name", placeholder: "Mac mini", value: "{current.label}", oninput: move |event| form.with_mut(|form| form.label = event.value()) } }
                label { "SSH target" input { "aria-label": "SSH target", placeholder: "user@host or SSH alias", value: "{current.target}", oninput: move |event| form.with_mut(|form| form.target = event.value()) } }
                label { "Identity file" input { "aria-label": "Identity file", placeholder: "SSH config default", value: "{current.identity_file}", oninput: move |event| form.with_mut(|form| form.identity_file = event.value()) } }
                label { "Port" input { r#type: "number", min: "1", max: "65535", "aria-label": "SSH port", placeholder: "Default", value: "{current.port}", oninput: move |event| form.with_mut(|form| form.port = event.value()) } }
                p { class: "muted", role: "status", {match &result {
                    Some(Check::Pending) => "Checking environment\u{2026}".to_owned(),
                    Some(Check::Failed(_)) => "1 of 1 failed".to_owned(),
                    Some(_) => "Connection checks passed".to_owned(),
                    None => "Check access before saving".to_owned(),
                }} }
                match &result {
                    Some(Check::Failed(error)) => rsx! { details { summary { "Show error" } p { "{error}" } } },
                    Some(Check::Local) => rsx! { p { class: "muted", "Already available locally" } },
                    Some(Check::Connected { platforms }) => rsx! { p { class: "muted", for (platform, available) in platforms.iter().copied() { span { key: "{logic::platform_wire(platform)}", "{logic::platform_name(platform)} {availability(available)} " } } } },
                    _ => rsx! {},
                }
                div { class: "dialog-actions",
                    button { r#type: "button", "aria-label": "Test host connection", disabled: !valid || checking, onclick: { let host = host.clone(); move |_| { if let Some(host) = host.clone() { test.call(host); } } }, if checking { "Checking\u{2026}" } else { "Test connection" } }
                    button { r#type: "button", "aria-label": "Cancel host editing", onclick: move |_| on_close.call(()), "Cancel" }
                    button { r#type: "button", "aria-label": "Save host", disabled: !valid || checking || current.label.trim().is_empty(), onclick: { let host = host.clone(); move |_| { if let Some(host) = host.clone() { save.call((host.clone(), false, original.clone().unwrap_or(host))); } } }, "Save host" }
                }
            }
        }
    }
}
