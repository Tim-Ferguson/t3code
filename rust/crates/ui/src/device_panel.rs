//! The device tab of the right panel (`DevicePanel.tsx`, `DeviceSetup.tsx`, `DeviceHostUpdates.tsx`, `DeviceWorkspace.tsx`,
//! `DeviceControlsRail.tsx`, `DeviceToolsPanel.tsx`, `useDeviceControls.ts`): the setup the first use asks for, the hosts and their
//! progress, the simulators and emulators each host lists (open, start, shut down), and a device that is open with its picture, quick
//! controls, settings and screenshot. The environment's device state is followed with `subscribeDeviceState`.
//!
//! An iOS simulator's picture is the hub's MJPEG stream in an `<img>`, and what the person does to it goes to the helper's socket
//! (`device_surface_abi.js`). Not ported: the H.264 decoding the original does for Android emulators and for iOS where it can (an Android
//! emulator shows a screenshot, refreshed on request), the 3D view and the Duo scenes, the accessibility overlay, the event log and the
//! floating player.
use crate::{
    media_save,
    preview_panel::{self, PageCmd, PageEvent, Script},
    right_panel,
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
    settings_common::connected_allows,
};
use dioxus::prelude::*;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use t3_client::{
    device_panel as logic,
    right_panel::{DeviceTarget, Surface},
};
use t3_contracts::{AuthEnvironmentScope, DeviceHostStatus, DevicePlatform, DeviceServiceState, DeviceSummary, Present};

/// The device state of each environment that has been read (`useDeviceState`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Model {
    by_environment: BTreeMap<String, DeviceServiceState>,
}

/// `EMPTY_DEVICE_STATE`: what the panel assumes until the first read.
fn empty_state() -> DeviceServiceState {
    serde_json::from_value(json!({"hosts": [], "hostStatus": "disabled", "hostStatuses": {}, "devices": [], "sessions": [], "onboardingCompleted": false, "agentAccessEnabled": false, "hubBasePath": "/api/device-hub", "revision": 0})).expect("the empty device state")
}
/// The environment's device state and whether it has been read, subscribing the caller.
pub(crate) fn current(state: Store<UiModel>, environment: &str) -> (DeviceServiceState, bool) {
    match state.devices().read().by_environment.get(environment) {
        Some(known) => (known.clone(), true),
        None => (empty_state(), false),
    }
}

/// Follows `subscribeDeviceState` while the component that calls this is shown and the connection can read it.
pub(crate) fn use_follow(state: Store<UiModel>, transport: TransportHandle, environment: String) {
    let live = use_memo(move || *state.status().read() == t3_client::connection::ConnectionStatus::Connected);
    let _ = use_resource(move || {
        let (transport, environment) = (transport.clone(), environment.clone());
        async move {
            if !live() {
                return;
            }
            loop {
                let Ok(mut stream) = runtime::request_stream(&transport, state, "subscribeDeviceState", json!({})) else { return };
                while let Some(batch) = stream.next().await {
                    let Ok(values) = batch else { break };
                    for value in values {
                        if let Some(next) = logic::state_of(&value) {
                            let mut field = state.devices();
                            field.write().by_environment.insert(environment.clone(), next);
                        }
                    }
                }
                runtime::delay(500).await;
                if *state.status().peek() != t3_client::connection::ConnectionStatus::Connected {
                    return;
                }
            }
        }
    });
}

/// `deviceEnvironment.list`, with whatever it needs of `input` (the hosts to retry, the tool to update).
pub(crate) async fn list(transport: TransportHandle, state: Store<UiModel>, input: Value) -> Result<(), String> {
    runtime::request_value_reported(transport, state, "device.list", input).await.map(|_| ())
}

fn hub_url(hub_base: &str, host: &str, platform: DevicePlatform, path: &str, params: &[(&str, &str)], ticket: Option<&str>) -> String {
    let mut full = logic::hub_path(&logic::vendor_base(hub_base, platform), host, path, params);
    if let Some(ticket) = ticket {
        full.push_str(&format!("&wsTicket={}", url::form_urlencoded::byte_serialize(ticket.as_bytes()).collect::<String>()));
    }
    full
}

// ---- the panel --------------------------------------------------------------------------------------------------------------

/// What choosing a device does: a running one is opened, a stopped one is started.
fn verb(device: &DeviceSummary) -> &'static str {
    if device.booted { "Open" } else { "Start" }
}

/// A device tab: the picker, or the device it holds.
#[component]
pub(crate) fn Panel(state: Store<UiModel>, transport: TransportHandle, environment_key: String, thread_id: String, surface: Surface) -> Element {
    use_follow(state, transport.clone(), environment_key.clone());
    let snapshot = use_memo({
        let environment = environment_key.clone();
        move || current(state, &environment)
    });
    let (dev, loaded) = snapshot();
    let owner = right_panel::Owner { environment: environment_key.clone(), thread: Some(thread_id.clone()) };
    let mut error = use_signal(|| None::<String>);
    let mut opening = use_signal(|| None::<DeviceSummary>);
    let mut refreshing = use_signal(|| false);
    let operate = connected_allows(state, AuthEnvironmentScope::OrchestrationOperate);
    let ready = use_memo({
        let environment = environment_key.clone();
        move || {
            let (dev, loaded) = current(state, &environment);
            loaded && dev.host_status != DeviceHostStatus::Disabled
        }
    });
    // Opening the panel lists the devices, which starts the hub where it is on; setup itself never installs or starts anything.
    use_effect({
        let transport = transport.clone();
        move || {
            if ready() {
                let transport = transport.clone();
                spawn(async move {
                    let _ = list(transport, state, json!({})).await;
                });
            }
        }
    });
    if logic::setup_needed(loaded, &dev) {
        return rsx! { div { class: "device-panel", Setup { state, transport: transport.clone(), environment_key: environment_key.clone(), dev: dev.clone(), owner: owner.clone(), surface_id: surface.id() } } };
    }
    let target = if let Surface::Device { target, .. } = &surface { target.clone() } else { None };
    let session = target.as_ref().and_then(|target| dev.sessions.iter().find(|session| session.thread_id.as_str() == thread_id && session.device_id.as_str() == target.device_id && session.host_id.as_str() == target.host_id));
    let active = session.and_then(|session| dev.devices.iter().find(|device| device.host_id == session.host_id && device.id == session.device_id)).cloned();
    let booting: Vec<String> = dev.booting_devices.present().into_iter().flatten().filter(|device| device.thread_id.as_str() == thread_id).map(|device| device.name.as_str().to_owned()).collect();
    let busy = logic::host_busy(&dev);
    let groups = logic::group_devices(&dev);
    let hub_base = dev.hub_base_path.clone();
    let select = {
        let (transport, owner, dev) = (transport.clone(), owner.clone(), dev.clone());
        use_callback(move |device: DeviceSummary| {
            let (transport, owner, dev) = (transport.clone(), owner.clone(), dev.clone());
            error.set(None);
            opening.set(Some(device.clone()));
            spawn(async move {
                let input = json!({"threadId": owner.thread, "hostId": device.host_id.as_str(), "deviceId": device.id.as_str(), "platform": logic::platform_wire(device.platform)});
                match runtime::request_value_reported(transport, state, "device.open", input).await {
                    Ok(_) => right_panel::open_device(state, &owner.environment, owner.thread.as_deref().unwrap_or_default(), DeviceTarget { host_id: device.host_id.as_str().to_owned(), device_id: device.id.as_str().to_owned(), platform: logic::platform_wire(device.platform).to_owned(), name: device.name.as_str().to_owned() }),
                    Err(message) => error.set(Some(message)),
                }
                let _ = dev;
                opening.set(None);
            });
        })
    };
    let shut_down = {
        let transport = transport.clone();
        use_callback(move |device: DeviceSummary| {
            let transport = transport.clone();
            error.set(None);
            spawn(async move {
                let input = json!({"hostId": device.host_id.as_str(), "deviceId": device.id.as_str(), "platform": logic::platform_wire(device.platform)});
                if let Err(message) = runtime::request_value_reported(transport, state, "device.shutdown", input).await {
                    error.set(Some(message));
                }
            });
        })
    };
    let close_tab = {
        let owner = owner.clone();
        let surface_id = surface.id();
        use_callback(move |_: ()| right_panel::close_surface(state, &owner, &surface_id))
    };
    let power_off = {
        let (transport, thread_id) = (transport.clone(), thread_id.clone());
        let (target, owner, surface_id) = (target.clone(), owner.clone(), surface.id());
        use_callback(move |_: ()| {
            let Some(target) = target.clone() else { return };
            let (transport, thread_id, owner, surface_id) = (transport.clone(), thread_id.clone(), owner.clone(), surface_id.clone());
            error.set(None);
            spawn(async move {
                let input = json!({"threadId": thread_id, "hostId": target.host_id, "deviceId": target.device_id, "shutdown": true});
                match runtime::request_value_reported(transport, state, "device.close", input).await {
                    Ok(_) => right_panel::close_surface(state, &owner, &surface_id),
                    Err(message) => error.set(Some(message)),
                }
            });
        })
    };
    rsx! {
        div { class: "device-panel",
            if ready() && active.is_none() && dev.host_status_detail.present().is_some() && logic::host_ready(&dev) {
                p { class: "device-note", role: "status", "{dev.host_status_detail.present().cloned().unwrap_or_default()}" }
            }
            HostUpdates { state, transport: transport.clone(), dev: dev.clone() }
            if !booting.is_empty() {
                p { class: "device-note", role: "status", "Starting {booting.join(\", \")}\u{2026} This can take a minute." }
            }
            if let Some(message) = error.read().clone() {
                div { class: "device-error", role: "alert",
                    p { "{message}" }
                    button { class: "icon-button", r#type: "button", "aria-label": "Dismiss device error", onclick: move |_| error.set(None), "\u{00d7}" }
                }
            }
            div { class: "device-body",
                if let Some(device) = active {
                    Workspace { key: "{device.host_id}:{device.id}", state, transport: transport.clone(), environment_key: environment_key.clone(), thread_id: thread_id.clone(), host_label: logic::host_label(&dev, device.host_id.as_str()).to_owned(), hub_base: hub_base.clone(), diagnostics: dev.host_status_detail.present().cloned(), device, on_close: close_tab, on_power_off: power_off }
                } else if opening.read().is_some() || busy || !loaded {
                    Loading { name: opening.read().as_ref().map_or("Devices".to_owned(), |device| device.name.as_str().to_owned()), description: opening.read().as_ref().map(|device| format!("{} \u{b7} {}", logic::host_label(&dev, device.host_id.as_str()), device.version)).unwrap_or_default(), message: logic::loading_message(&dev, opening.read().as_ref()) }
                } else {
                    div { class: "device-picker",
                        if groups.is_empty() {
                            p { class: "muted", "{logic::empty_picker_message(&dev)}" }
                        }
                        if logic::host_ready(&dev) {
                            for (platform, devices) in groups.iter() {
                                section { key: "{logic::platform_wire(*platform)}", class: "device-group",
                                    h3 { "{logic::platform_group_label(*platform)}" }
                                    div { class: "device-list", role: "list",
                                        for device in devices.iter().cloned() {
                                            div { key: "{device.host_id}:{device.id}", class: "device-row", role: "listitem",
                                                button { class: "device-open", r#type: "button", "aria-label": "{verb(&device)} {device.name}", disabled: opening.read().is_some() || !operate,
                                                    onclick: { let device = device.clone(); move |_| select.call(device.clone()) },
                                                    strong { "{device.name}" } span { class: "muted", "{logic::device_description(&dev, &device)}" }
                                                    span { class: "device-action", "{verb(&device)}" } }
                                                if device.booted {
                                                    button { class: "device-shutdown", r#type: "button", "aria-label": "Shut down {device.name}", title: "Shut down {device.name}", disabled: !operate,
                                                        onclick: { let device = device.clone(); move |_| shut_down.call(device.clone()) }, "Shut down" }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        if logic::needs_android_hint(&dev) {
                            p { class: "muted", "No Android virtual devices found. Create one in Android Studio's Device Manager, then refresh." }
                        }
                        if loaded && !busy {
                            button { r#type: "button", "aria-label": "Refresh devices", disabled: *refreshing.read(), onclick: { let transport = transport.clone(); move |_| {
                                let transport = transport.clone();
                                refreshing.set(true);
                                spawn(async move {
                                    if let Err(message) = list(transport, state, json!({})).await { error.set(Some(message)); }
                                    refreshing.set(false);
                                });
                            } }, "Refresh devices" }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn Loading(name: String, description: String, message: String) -> Element {
    rsx! {
        div { class: "device-loading", role: "status",
            strong { "{name}" }
            if !description.is_empty() { span { class: "muted", "{description}" } }
            span { class: "muted", "{message}" }
        }
    }
}

/// `DeviceHostUpdates`: a host that is installing, starting or failed says so, and a failed one can be tried again.
#[component]
pub(crate) fn HostUpdates(state: Store<UiModel>, transport: TransportHandle, dev: DeviceServiceState) -> Element {
    let mut pending = use_signal(|| None::<String>);
    if dev.host_status == DeviceHostStatus::Disabled {
        return rsx! {};
    }
    let retry = dev.supports_host_retry.present() == Some(&true);
    rsx! {
        div { class: "device-hosts",
            for host in dev.hosts.iter() {
                if let Some(status) = dev.host_statuses.get(&host.id).filter(|status| matches!(status.status, DeviceHostStatus::Installing | DeviceHostStatus::Starting | DeviceHostStatus::Failed)) {
                    {
                        let failed = status.status == DeviceHostStatus::Failed;
                        let detail = status.detail.present().cloned().unwrap_or_else(|| if failed { "Device support could not start.".into() } else if status.status == DeviceHostStatus::Installing { "Installing device tools\u{2026}".into() } else { "Starting device tools\u{2026}".into() });
                        let id = host.id.as_str().to_owned();
                        let transport = transport.clone();
                        rsx! {
                            div { key: "{host.id}", class: "device-host", role: if failed { "alert" } else { "status" }, "data-host": "{host.id}",
                                div { strong { "{host.label}" } p { class: "muted", "{detail}" }
                                    if failed { p { class: "muted", "Check the host connection and network access, then retry. Your device settings are saved." } } }
                                if failed && retry {
                                    button { r#type: "button", "aria-label": "Retry {host.label}", disabled: pending.read().is_some(), onclick: move |_| {
                                        let (transport, id) = (transport.clone(), id.clone());
                                        pending.set(Some(id.clone()));
                                        spawn(async move {
                                            let _ = list(transport, state, json!({"retryHostId": id})).await;
                                            pending.set(None);
                                        });
                                    }, if pending.read().as_deref() == Some(host.id.as_str()) { "Retrying\u{2026}" } else { "Retry" } }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

// ---- setup ------------------------------------------------------------------------------------------------------------------

/// `DeviceSetup`: the three steps before the first device, in the panel itself. Cancel closes the tab.
#[component]
fn Setup(state: Store<UiModel>, transport: TransportHandle, environment_key: String, dev: DeviceServiceState, owner: right_panel::Owner, surface_id: String) -> Element {
    let mut step = use_signal(|| 0usize);
    let mut pending = use_signal(|| None::<&'static str>);
    let mut checking = use_signal(|| false);
    let can_configure = connected_allows(state, AuthEnvironmentScope::SettingsWrite);
    let enabled = dev.host_status != DeviceHostStatus::Disabled;
    let busy = matches!(dev.host_status, DeviceHostStatus::Installing | DeviceHostStatus::Starting);
    let local_unavailable = dev.hosts.iter().any(|host| host.kind == t3_contracts::DeviceHostKind::Local && !host.platforms.iter().any(|platform| platform.available));
    let update = {
        let transport = transport.clone();
        use_callback(move |(kind, input): (&'static str, Value)| {
            if !can_configure {
                return;
            }
            let transport = transport.clone();
            pending.set(Some(kind));
            spawn(async move {
                let _ = runtime::request_value_reported(transport, state, "device.configure", input).await;
                pending.set(None);
            });
        })
    };
    let at = step();
    let hub_pending = pending() == Some("hub") || (busy && pending() != Some("agent"));
    rsx! {
        section { class: "device-setup", "aria-label": "Set up devices",
            h2 { "Set up devices" }
            p { class: "muted", "Review what runs on this environment before using simulators and emulators." }
            ol { class: "device-steps", for (index, name) in ["Device hub", "Simulators", "Agent access"].into_iter().enumerate() { li { key: "{name}", "aria-current": if index == at { "step" } else { "false" }, "{name}" } } }
            HostUpdates { state, transport: transport.clone(), dev: dev.clone() }
            if at == 0 {
                div { class: "setting-row",
                    div { class: "font-description", strong { "Enable the device hub" } p { class: "muted", "{logic::DEVICE_HUB_DESCRIPTION}" } }
                    div { class: "font-controls", input { r#type: "checkbox", role: "switch", "aria-label": "Enable device hub", checked: enabled, disabled: !can_configure || busy || pending().is_some(),
                        onchange: move |event| { let on = event.checked(); update.call(("hub", if on { json!({"enabled": true}) } else { json!({"enabled": false, "agentAccessEnabled": false}) })); } } }
                }
                if hub_pending { p { class: "muted", role: "status", "{logic::hub_progress(dev.host_status, false)}" } }
                else if dev.host_status == DeviceHostStatus::Ready { p { class: "muted", role: "status", "Device hub is ready." } }
            }
            if at == 1 || (at == 0 && enabled && local_unavailable) {
                div { class: "device-platforms",
                    h3 { "Check simulator support" }
                    for platform in [DevicePlatform::Ios, DevicePlatform::Android] {
                        {
                            let (ready, message) = logic::platform_setup_status(&dev, platform);
                            rsx! { div { key: "{logic::platform_wire(platform)}", class: if ready { "device-platform ready" } else { "device-platform" }, strong { "{logic::platform_name(platform)}" } p { class: "muted", "{message}" } } }
                        }
                    }
                    p { class: "muted", "You can use either platform. Fixing a missing platform does not block the other one." }
                    button { r#type: "button", "aria-label": "Check again", disabled: !enabled || busy || pending().is_some() || checking(), onclick: { let transport = transport.clone(); move |_| { let transport = transport.clone(); checking.set(true); spawn(async move { let _ = list(transport, state, json!({})).await; checking.set(false); }); } }, if checking() { "Checking\u{2026}" } else { "Check again" } }
                }
            }
            if at == 2 {
                div { class: "setting-row",
                    div { class: "font-description", strong { "Allow agent control" } p { class: "muted", "{logic::AGENT_DEVICE_DESCRIPTION}" } }
                    div { class: "font-controls", input { r#type: "checkbox", role: "switch", "aria-label": "Allow agents to control devices", checked: dev.agent_access_enabled, disabled: !can_configure || !enabled || busy || pending().is_some(),
                        onchange: move |event| update.call(("agent", json!({"agentAccessEnabled": event.checked()}))) } }
                }
                if pending() == Some("agent") { p { class: "muted", role: "status", "{logic::agent_progress(dev.host_status, false)}" } }
                else if logic::agent_tools_ready(&dev) { p { class: "muted", role: "status", "Agent tools are ready." } }
                p { class: "muted", "Leave this off to keep manual device controls without giving agents access." }
            }
            if dev.host_status == DeviceHostStatus::Failed {
                if let Some(detail) = dev.host_status_detail.present() { p { class: "error-banner", role: "alert", "{detail}" } }
            }
            div { class: "device-setup-actions",
                if at == 0 {
                    button { r#type: "button", "aria-label": "Cancel setup", onclick: { let owner = owner.clone(); let id = surface_id.clone(); move |_| right_panel::close_surface(state, &owner, &id) }, "Cancel" }
                } else {
                    button { r#type: "button", "aria-label": "Back to the previous step", disabled: busy || pending().is_some(), onclick: move |_| step.set(at - 1), "Back" }
                }
                if at < 2 {
                    button { r#type: "button", "aria-label": "Continue to the next step", disabled: dev.host_status != DeviceHostStatus::Ready || pending().is_some(), onclick: move |_| step.set(at + 1), "Continue" }
                } else {
                    button { r#type: "button", "aria-label": "Finish device setup", disabled: !can_configure || dev.host_status != DeviceHostStatus::Ready || pending().is_some(), onclick: move |_| update.call(("complete", json!({"onboardingCompleted": true}))), if pending() == Some("complete") { "Saving\u{2026}" } else { "Done" } }
                }
            }
        }
    }
}

// ---- an open device ---------------------------------------------------------------------------------------------------------

/// What the device's quick controls and tools drawer read of it (`DeviceDetail`), and the action in flight.
#[derive(Debug, Clone, PartialEq, Default)]
struct Controls {
    detail: Option<Value>,
    pending: bool,
    error: Option<String>,
}

const IOS_BUTTON: u8 = 0x04;
const IOS_ORIENTATION: u8 = 0x07;
const IOS_ORIENTATIONS: [&str; 4] = ["portrait", "landscape_left", "portrait_upside_down", "landscape_right"];

#[component]
fn Workspace(state: Store<UiModel>, transport: TransportHandle, environment_key: String, thread_id: String, host_label: String, hub_base: String, diagnostics: Option<String>, device: DeviceSummary, on_close: Callback<()>, on_power_off: Callback<()>) -> Element {
    let host = device.host_id.as_str().to_owned();
    let id = device.id.as_str().to_owned();
    let platform = device.platform;
    let mut controls = use_signal(Controls::default);
    let mut tools = use_signal(|| false);
    let mut capturing = use_signal(|| false);
    let mut picture = use_signal(|| None::<String>);
    let mut input_connected = use_signal(|| false);
    let mut snapshot = use_signal(|| None::<String>);
    let surface = use_hook(|| format!("device-surface-{}", uuid::Uuid::new_v4()));
    let link = use_hook(preview_panel::Link::default);
    let read_detail = {
        let (transport, host, id) = (transport.clone(), host.clone(), id.clone());
        use_callback(move |_: ()| {
            let (transport, host, id) = (transport.clone(), host.clone(), id.clone());
            spawn(async move {
                match runtime::request_value_reported(transport, state, "device.detail", json!({"hostId": host, "deviceId": id})).await {
                    Ok(detail) => controls.with_mut(|controls| { controls.detail = Some(detail); controls.error = None; }),
                    Err(message) => controls.with_mut(|controls| { controls.detail = None; controls.error = Some(message); }),
                }
            });
        })
    };
    use_hook(|| read_detail.call(()));
    // The picture and, for iOS, the socket that takes the person's touches: both want the hub's ticket.
    use_future({
        let (transport, hub_base, host, id, link, surface) = (transport.clone(), hub_base.clone(), host.clone(), id.clone(), link.clone(), surface.clone());
        move || {
            let (transport, hub_base, host, id, link, surface) = (transport.clone(), hub_base.clone(), host.clone(), id.clone(), link.clone(), surface.clone());
            async move {
                if platform != DevicePlatform::Ios {
                    return;
                }
                let ticket = runtime::media_ticket(&transport, state).await.ok().flatten();
                let image = logic::mjpeg_path(&hub_base, &host, &id);
                let image = match &ticket {
                    Some(ticket) => format!("{image}&wsTicket={}", url::form_urlencoded::byte_serialize(ticket.as_bytes()).collect::<String>()),
                    None => image,
                };
                picture.set(runtime::environment_http_url(&transport, image.trim_start_matches('/')));
                runtime::delay(0).await;
                let mut page = preview_panel::open_page(Script::Device, &surface, false);
                link.set_sender(Some(page.commands.clone()));
                let socket = hub_url(&hub_base, &host, platform, "/helper/ws", &[("device", id.as_str())], ticket.as_deref());
                if let Some(socket) = runtime::environment_socket_url(&transport, socket.trim_start_matches('/')) {
                    let _ = page.commands.unbounded_send(PageCmd::Connect(socket));
                }
                while let Some(event) = page.events.next().await {
                    match event {
                        PageEvent::Open => input_connected.set(true),
                        PageEvent::Close { .. } => input_connected.set(false),
                        _ => {}
                    }
                }
            }
        }
    });
    use_drop({
        let link = link.clone();
        move || link.stop()
    });
    let act = {
        let (transport, host, id) = (transport.clone(), host.clone(), id.clone());
        use_callback(move |action: Value| {
            let (transport, host, id) = (transport.clone(), host.clone(), id.clone());
            if controls.peek().pending || controls.peek().detail.is_none() {
                return;
            }
            controls.with_mut(|controls| { controls.pending = true; controls.error = None; });
            spawn(async move {
                match runtime::request_value_reported(transport, state, "device.action", logic::action_input(&host, &id, action)).await {
                    Ok(detail) => controls.with_mut(|controls| controls.detail = Some(detail)),
                    Err(message) => controls.with_mut(|controls| controls.error = Some(message)),
                }
                controls.with_mut(|controls| controls.pending = false);
            });
        })
    };
    let screenshot = {
        let (transport, hub_base, host, id, name) = (transport.clone(), hub_base.clone(), host.clone(), id.clone(), device.name.as_str().to_owned());
        use_callback(move |save: bool| {
            if *capturing.peek() {
                return;
            }
            let (transport, hub_base, host, id, name) = (transport.clone(), hub_base.clone(), host.clone(), id.clone(), name.clone());
            capturing.set(true);
            controls.with_mut(|controls| controls.error = None);
            spawn(async move {
                let path = logic::screenshot_path(&hub_base, &host, platform, &id);
                match runtime::environment_bytes(&transport, state, reqwest::Method::POST, path.trim_start_matches('/'), None).await {
                    Ok(bytes) if bytes.is_empty() => controls.with_mut(|controls| controls.error = Some("The device returned an empty screenshot. Try again.".into())),
                    Ok(bytes) if save => media_save::save(state, &logic::screenshot_file_name(&name, chrono::Utc::now().timestamp_millis()), bytes).await,
                    Ok(bytes) => {
                        use base64::Engine;
                        snapshot.set(Some(format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes))));
                    }
                    Err(message) => controls.with_mut(|controls| controls.error = Some(if message.starts_with("Server returned") { "Screenshot capture failed. Try again.".into() } else { message })),
                }
                capturing.set(false);
            });
        })
    };
    let current = controls.read().clone();
    let settings = current.detail.as_ref().map(|detail| detail["settings"].clone()).unwrap_or(Value::Null);
    let appearance = settings["appearance"].as_str().map(str::to_owned);
    let next_appearance = logic::next_appearance(appearance.as_deref());
    let controls_off = current.pending || current.detail.is_none();
    let rotate_link = link.clone();
    let home_link = link.clone();
    let name = device.name.as_str().to_owned();
    rsx! {
        div { class: "device-workspace",
            div { class: "device-view",
                if let Some(message) = current.error.clone().filter(|_| !tools()) { p { class: "error-banner", role: "alert", "{message}" } }
                div { id: "{surface}", class: "device-screen", "aria-label": "{name} screen",
                    if platform == DevicePlatform::Ios {
                        if let Some(src) = picture.read().clone() {
                            img { class: "device-picture", alt: "{name}", src: "{src}" }
                        } else { img { class: "device-picture", alt: "{name}" } p { class: "muted", role: "status", "Connecting video\u{2026}" } }
                    } else {
                        if let Some(src) = snapshot.read().clone() {
                            img { class: "device-picture", alt: "{name}", src: "{src}" }
                        } else { p { class: "muted", role: "status", "Live video for Android emulators is not available in this app yet. Refresh the picture to see the screen." } }
                    }
                }
                aside { class: "device-rail", "aria-label": "Device controls", "data-device": "{name}",
                    button { class: "icon-button", r#type: "button", "aria-label": "Home", title: "Home", disabled: platform != DevicePlatform::Ios || !input_connected(), onclick: move |_| { home_link.send_tagged(IOS_BUTTON, json!({"button": "home"})); }, "\u{2302}" }
                    button { class: "icon-button", r#type: "button", "aria-label": "Rotate device", title: "Rotate device", disabled: if platform == DevicePlatform::Ios { !input_connected() } else { controls_off },
                        onclick: { let act = act.clone(); let orientation = settings["orientation"].as_str().map(str::to_owned); move |_| {
                            if platform == DevicePlatform::Ios {
                                let at = orientation.as_deref().and_then(|current| IOS_ORIENTATIONS.iter().position(|candidate| *candidate == current)).unwrap_or(0);
                                rotate_link.send_tagged(IOS_ORIENTATION, json!({"orientation": IOS_ORIENTATIONS[(at + 1) % IOS_ORIENTATIONS.len()]}));
                            } else {
                                act.call(json!({"type": "setOrientation", "value": "landscape_left"}));
                            }
                        } }, "\u{27f3}" }
                    button { class: "icon-button", r#type: "button", "aria-label": "Switch device to {next_appearance} mode", title: "Switch device to {next_appearance} mode", disabled: controls_off || appearance.is_none(), onclick: { let act = act.clone(); move |_| act.call(json!({"type": "setAppearance", "value": next_appearance})) }, if appearance.as_deref() == Some("dark") { "\u{2600}" } else { "\u{263e}" } }
                    select { "aria-label": "Device text size", title: "Device text size", disabled: controls_off || settings["textSize"].is_null(), value: "{settings[\"textSize\"].as_str().unwrap_or_default()}",
                        onchange: { let act = act.clone(); move |event| act.call(json!({"type": "setTextSize", "value": event.value()})) },
                        for (value, label) in logic::TEXT_SIZES { option { key: "{value}", value: "{value}", selected: settings["textSize"].as_str() == Some(value), "{label}" } } }
                    button { class: "icon-button", r#type: "button", "aria-label": "Device tools", title: "Device tools", "aria-pressed": "{tools()}", onclick: move |_| tools.set(!tools()), "\u{2699}" }
                    button { class: "icon-button", r#type: "button", "aria-label": if capturing() { "Capturing screenshot" } else { "Save screenshot" }, title: "Save screenshot", disabled: capturing(), onclick: move |_| screenshot.call(true), "\u{1f4f7}" }
                    if platform == DevicePlatform::Android {
                        button { class: "icon-button", r#type: "button", "aria-label": "Refresh picture", title: "Refresh picture", disabled: capturing(), onclick: move |_| screenshot.call(false), "\u{21bb}" }
                    }
                    button { class: "icon-button", r#type: "button", "aria-label": "Close device panel", title: "Close device panel", onclick: move |_| on_close.call(()), "\u{00d7}" }
                    button { class: "icon-button destructive", r#type: "button", "aria-label": "Power off device", title: "Power off device", onclick: move |_| on_power_off.call(()), "\u{23fb}" }
                }
            }
            if tools() {
                Tools { device: device.clone(), host_label: host_label.clone(), diagnostics: diagnostics.clone(), controls, act, on_close: move |_| tools.set(false) }
            }
        }
    }
}

/// `DeviceToolsPanel`, as far as it is a form: what the device's settings are, and the things that can be done to it.
#[component]
fn Tools(device: DeviceSummary, host_label: String, diagnostics: Option<String>, controls: Signal<Controls>, act: Callback<Value>, on_close: EventHandler<()>) -> Element {
    let mut url = use_signal(String::new);
    let mut app = use_signal(String::new);
    let mut push = use_signal(|| "{\"aps\":{\"alert\":\"Hello\"}}".to_owned());
    let current = controls.read().clone();
    let settings = current.detail.as_ref().map(|detail| detail["settings"].clone()).unwrap_or(Value::Null);
    let foreground = current.detail.as_ref().and_then(|detail| detail["foregroundApp"]["id"].as_str().map(str::to_owned));
    let off = current.pending || current.detail.is_none();
    let toggles = [("reduceMotion", "Reduce motion"), ("increaseContrast", "Increase contrast"), ("reduceTransparency", "Reduce transparency"), ("showBorders", "Show button shapes"), ("voiceOver", "VoiceOver"), ("networkEnabled", "Network")];
    let ios = device.platform == DevicePlatform::Ios;
    rsx! {
        aside { class: "device-tools", "aria-label": "Device tools drawer", "data-device": "{device.name}",
            div { class: "device-tools-head", strong { "{device.name}" } span { class: "muted", "{host_label} \u{b7} {device.version}" }
                button { class: "icon-button", r#type: "button", "aria-label": "Close device tools", onclick: move |_| on_close.call(()), "\u{00d7}" } }
            if let Some(message) = current.error.clone() { p { class: "error-banner", role: "alert", "{message}" } }
            if let Some(diagnostics) = diagnostics { details { summary { "Host diagnostics" } p { class: "muted", "{diagnostics}" } } }
            if let Some(app) = foreground { p { class: "muted", "Foreground app: {app}" } }
            div { class: "device-toggles",
                for (setting, label) in toggles {
                    if !settings[setting].is_null() && (ios || matches!(setting, "networkEnabled")) {
                        label { key: "{setting}", class: "device-toggle", input { r#type: "checkbox", role: "switch", "aria-label": "{label}", checked: settings[setting].as_bool().unwrap_or(false), disabled: off,
                            onchange: move |event| act.call(json!({"type": "setToggle", "setting": setting, "value": event.checked()})) } " {label}" }
                    }
                }
            }
            div { class: "device-field",
                input { "aria-label": "URL to open on the device", placeholder: "https://example.com", value: "{url}", oninput: move |event| url.set(event.value()) }
                button { r#type: "button", "aria-label": "Open URL on the device", disabled: off || url.read().trim().is_empty(), onclick: move |_| act.call(json!({"type": "openUrl", "url": url.read().trim()})), "Open URL" }
            }
            div { class: "device-field",
                input { "aria-label": "App identifier", placeholder: "com.example.app", value: "{app}", oninput: move |event| app.set(event.value()) }
                button { r#type: "button", "aria-label": "Launch app", disabled: off || app.read().trim().is_empty(), onclick: move |_| act.call(json!({"type": "launchApp", "appId": app.read().trim()})), "Launch" }
                button { r#type: "button", "aria-label": "Terminate app", disabled: off || app.read().trim().is_empty(), onclick: move |_| act.call(json!({"type": "terminateApp", "appId": app.read().trim()})), "Terminate" }
            }
            if ios {
                div { class: "device-field",
                    textarea { "aria-label": "Push notification payload", value: "{push}", oninput: move |event| push.set(event.value()) }
                    button { r#type: "button", "aria-label": "Send push", disabled: off || app.read().trim().is_empty() || serde_json::from_str::<Value>(&push.read()).is_err(), onclick: move |_| {
                        if let Ok(payload) = serde_json::from_str::<Value>(&push.read()) { act.call(json!({"type": "sendPush", "appId": app.read().trim(), "payload": payload})); }
                    }, "Send push" }
                }
                button { r#type: "button", "aria-label": "Shake device", disabled: off, onclick: move |_| act.call(json!({"type": "shake"})), "Shake" }
            }
        }
    }
}
