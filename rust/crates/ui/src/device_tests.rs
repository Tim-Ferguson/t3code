//! The device tab through the mounted UI and the real native server: the setup the first use asks for, a host that fails and is tried
//! again, the picker, starting and opening a simulator, its controls and settings (through the server's own device service over the hub
//! and helper fixtures of its tests: nothing starts a real simulator), a screenshot read from the hub, and closing and powering off.
use super::diff_tests::{Fixture, open_thread, rendered_text, seed, until};
use super::*;
use crate::{
    diff_panel::Gate,
    media_save::SAVED,
    preview_panel::{PageCmd, TEST_PAGE},
    right_panel::MEMORY,
    runtime::{
        keys_tests::{chord, poll_until, press},
        transport_tests::{click_control, click_when_enabled, control, control_attribute},
    },
};
use async_trait::async_trait;
use std::sync::{Arc, Mutex, atomic::{AtomicUsize, Ordering}};
use t3_client::keybindings::Dom;
use t3_contracts::*;
use t3_server::{
    device_host::DeviceHost,
    device_service::DeviceService,
    local_device_host::{DeviceHostAgentReady, DeviceHostReady, DevicePhase},
    ssh_device_host::HostError,
};

/// A host that cannot be reached, which is asked again each time.
struct Unreachable {
    attempts: Arc<AtomicUsize>,
}
#[async_trait]
impl DeviceHost for Unreachable {
    fn id(&self) -> DeviceHostId {
        DeviceHostId::new("mini").unwrap()
    }
    async fn summary(&self) -> DeviceHostSummary {
        serde_json::from_value(json!({"id": "mini", "kind": "ssh", "label": "Mac mini", "platforms": [{"platform": "ios", "available": false, "reason": "Not connected"}], "hubInstalled": false, "agentDeviceInstalled": false})).unwrap()
    }
    async fn inspect(&self) -> Result<DeviceHostSummary, HostError> {
        Ok(self.summary().await)
    }
    async fn platform(&self, platform: DevicePlatform) -> DevicePlatformAvailability {
        serde_json::from_value(json!({"platform": platform_wire(platform), "available": false, "reason": "Not connected"})).unwrap()
    }
    async fn ensure_ready(&self, _: DevicePhase) -> Result<DeviceHostReady, DeviceHostUnavailableError> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        Err(serde_json::from_value(json!({"_tag": "DeviceHostUnavailableError", "hostId": "mini", "reason": "ssh: connect to host mini port 22: Connection refused"})).unwrap())
    }
    async fn ensure_agent_ready(&self, _: DevicePhase) -> Result<DeviceHostAgentReady, DeviceHostUnavailableError> {
        Err(serde_json::from_value(json!({"_tag": "DeviceHostUnavailableError", "hostId": "mini", "reason": "not connected"})).unwrap())
    }
    fn current(&self) -> Option<DeviceHostReady> {
        None
    }
    fn current_agent(&self) -> Option<t3_server::device_agent_daemon::AgentDeviceEndpoint> {
        None
    }
    fn generation(&self) -> u64 {
        0
    }
    async fn stop_agent(&self) {}
    async fn stop(&self) {}
    async fn shutdown(&self) {}
}
fn platform_wire(platform: DevicePlatform) -> &'static str {
    t3_client::device_panel::platform_wire(platform)
}

const SCOPES: [AuthEnvironmentScope; 4] = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::FilesystemRead, AuthEnvironmentScope::SettingsWrite];

async fn ready(fixture: &Fixture) -> (VirtualDom, super::diff_tests::Harness, Store<UiModel>) {
    MEMORY.with(|memory| memory.borrow_mut().clear());
    SAVED.with(|saved| saved.borrow_mut().clear());
    let (mut dom, props) = fixture.client(Gate::default(), &SCOPES);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    dom.in_scope(ScopeId::APP, || spawn(crate::right_panel::hydrate(state)));
    crate::runtime::diff_tests::pump(&mut dom).await;
    (dom, props, state)
}
async fn open_device_tab(dom: &mut VirtualDom, props: &super::diff_tests::Harness, state: Store<UiModel>) {
    open_thread(dom, props, state, "thread-a").await;
    assert_eq!(press(dom, chord("b", false, true), Dom::default()).await.as_deref(), Some("rightPanel.toggle"));
    until(dom, "the launcher", |dom| control(dom, "Open Device").is_some()).await;
    click_when_enabled(dom, "Open Device").await;
}
fn service(fixture: &Fixture) -> DeviceService {
    fixture.devices.clone().expect("a fixture with devices")
}
/// What the hub's double reports, and for the emulator one more (a profile the fixture reads on each request).
fn hub_has_emulator(fixture: &Fixture) {
    std::fs::write(fixture.device_files.join("profile.json"), r#"{"androidId":"emulator-5554"}"#).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn the_first_use_walks_through_setup_and_a_host_that_fails_can_be_tried_again() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let fixture = Fixture::start_with_devices(vec![Arc::new(Unreachable { attempts: attempts.clone() })]).await;
    let (mut dom, props, state) = ready(&fixture).await;
    open_device_tab(&mut dom, &props, state).await;
    until(&mut dom, "the setup", |dom| rendered_text(dom).contains("Set up devices") && control(dom, "Enable device hub").is_some()).await;
    // Nothing is installed or started by looking at the setup.
    assert_eq!(service(&fixture).snapshot().host_status, DeviceHostStatus::Disabled);
    assert!(control(&dom, "Continue to the next step").map(|(element, _)| element).is_some());
    crate::runtime::transport_tests::change_control(&mut dom, "Enable device hub", "true");
    // The hub starts on this machine; the host that cannot be reached says why and offers to try again.
    until(&mut dom, "the failed host", |dom| control(dom, "Retry Mac mini").is_some() && rendered_text(dom).contains("Connection refused")).await;
    assert!(rendered_text(&dom).contains("Check the host connection and network access, then retry."));
    let before = attempts.load(Ordering::SeqCst);
    click_control(&mut dom, "Retry Mac mini");
    poll_until(&mut dom, "the host to be asked again", |_| attempts.load(Ordering::SeqCst) > before).await;
    until(&mut dom, "ready", |dom| control_attribute(dom, "Continue to the next step", "disabled").and_then(|(_, value)| value).as_deref() != Some("true")).await;
    click_control(&mut dom, "Continue to the next step");
    until(&mut dom, "the platforms", |dom| rendered_text(dom).contains("Check simulator support") && control(dom, "Check again").is_some()).await;
    assert!(rendered_text(&dom).contains("You can use either platform."));
    click_control(&mut dom, "Continue to the next step");
    until(&mut dom, "agent access", |dom| control(dom, "Allow agents to control devices").is_some()).await;
    click_control(&mut dom, "Finish device setup");
    until(&mut dom, "the picker", |dom| control(dom, "Refresh devices").is_some() && !rendered_text(dom).contains("Set up devices")).await;
    assert!(service(&fixture).snapshot().onboarding_completed);
    // The simulator the hub lists, stopped, is offered to start.
    until(&mut dom, "the simulator", |dom| control(dom, "Start Fixture iPhone").is_some()).await;
    assert!(rendered_text(&dom).contains("iOS Simulators") && rendered_text(&dom).contains("This machine"));
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_simulator_is_started_opened_controlled_photographed_and_powered_off() {
    let fixture = Fixture::start_with_devices(vec![]).await;
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = seen.clone();
    TEST_PAGE.with(|page| *page.borrow_mut() = Some(Box::new(move |_events, mut commands| {
        let sink = sink.clone();
        tokio::spawn(async move {
            while let Some(command) = futures_util::StreamExt::next(&mut commands).await {
                if let PageCmd::Connect(url) = command {
                    sink.lock().unwrap().push(url);
                }
            }
        });
    })));
    service(&fixture).configure(serde_json::from_value(json!({"enabled": true, "onboardingCompleted": true})).unwrap()).await.unwrap();
    let (mut dom, props, state) = ready(&fixture).await;
    open_device_tab(&mut dom, &props, state).await;
    until(&mut dom, "the simulator", |dom| control(dom, "Start Fixture iPhone").is_some()).await;
    click_control(&mut dom, "Start Fixture iPhone");
    // The device opens in the tab that was the picker: its name is the tab's, and the server holds the session.
    until(&mut dom, "the device", |dom| control(dom, "Device controls").is_some() && control(dom, "Close Fixture iPhone tab").is_some()).await;
    assert_eq!(service(&fixture).snapshot().sessions.len(), 1);
    // The picture is the hub's, and the input socket asked for is the helper's, with the ticket and the host.
    until(&mut dom, "the input socket", |_| !seen.lock().unwrap().is_empty()).await;
    let socket = seen.lock().unwrap()[0].clone();
    assert!(socket.starts_with("ws://") && socket.contains("/api/device-hub/vendor/serve-sim/helper/ws?device=fixture-ios&hostId=local&wsTicket="), "{socket}");
    let environment = state.peek().destination.as_ref().unwrap().to_string();
    let ids = state.peek().right_panel.panels.get(&format!("{environment}:thread-a")).surfaces.iter().map(|surface| surface.id()).collect::<Vec<_>>();
    assert_eq!(ids, ["device:local:fixture-ios"]);

    // A quick control is an action the server runs on the device, and the answer is what the button then says.
    until(&mut dom, "its settings", |dom| control_attribute(dom, "Switch device to dark mode", "disabled").and_then(|(_, value)| value).as_deref() != Some("true") && control(dom, "Switch device to dark mode").is_some()).await;
    click_control(&mut dom, "Switch device to dark mode");
    until(&mut dom, "dark", |dom| control(dom, "Switch device to light mode").is_some()).await;

    // The tools drawer shows the settings and says what the server refused.
    click_control(&mut dom, "Device tools");
    until(&mut dom, "the drawer", |dom| control(dom, "Device tools").is_some() && control(dom, "Shake device").is_some()).await;
    click_when_enabled(&mut dom, "Shake device").await;
    until(&mut dom, "the refusal", |dom| rendered_text(dom).contains("The server could not complete this request.")).await;

    // A screenshot is read from the hub and kept.
    click_control(&mut dom, "Save screenshot");
    poll_until(&mut dom, "the screenshot to be kept", |_| !SAVED.with(|saved| saved.borrow().is_empty())).await;
    let (name, bytes) = SAVED.with(|saved| saved.borrow()[0].clone());
    assert!(name.starts_with("Fixture-iPhone-") && name.ends_with(".png"), "{name}");
    assert_eq!(bytes, b"\x89PNG\r\n\x1a\nfixture screenshot");

    // Powering off closes the session and the tab.
    click_control(&mut dom, "Power off device");
    until(&mut dom, "the tab is gone", |dom| control(dom, "Device controls").is_none() && control(dom, "Close Fixture iPhone tab").is_none()).await;
    assert!(service(&fixture).snapshot().sessions.is_empty());
    TEST_PAGE.with(|page| *page.borrow_mut() = None);
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn both_platforms_are_listed_and_what_the_server_refuses_is_said_and_can_be_dismissed() {
    let fixture = Fixture::start_with_devices(vec![]).await;
    hub_has_emulator(&fixture);
    service(&fixture).configure(serde_json::from_value(json!({"enabled": true, "onboardingCompleted": true})).unwrap()).await.unwrap();
    let (mut dom, props, state) = ready(&fixture).await;
    open_device_tab(&mut dom, &props, state).await;
    until(&mut dom, "both platforms", |dom| control(dom, "Start Fixture iPhone").is_some() && control(dom, "Open Fixture Pixel").is_some()).await;
    assert!(rendered_text(&dom).contains("Android Emulators"));
    // Only the running device can be shut down from the list.
    assert!(control(&dom, "Shut down Fixture Pixel").is_some() && control(&dom, "Shut down Fixture iPhone").is_none());
    // This machine cannot run an emulator, and the server says so; the list stays, and the line goes when it is dismissed.
    click_control(&mut dom, "Open Fixture Pixel");
    until(&mut dom, "the refusal", |dom| control(dom, "Dismiss device error").is_some()).await;
    assert!(control(&dom, "Open Fixture Pixel").is_some(), "the list is still there");
    click_control(&mut dom, "Dismiss device error");
    until(&mut dom, "dismissed", |dom| control(dom, "Dismiss device error").is_none()).await;
    assert!(service(&fixture).snapshot().sessions.is_empty());

    // A simulator that was started stays running when its tab is closed, and is shut down from the list.
    click_control(&mut dom, "Start Fixture iPhone");
    until(&mut dom, "the device", |dom| control(dom, "Device controls").is_some()).await;
    click_control(&mut dom, "Close device panel");
    until(&mut dom, "no panel", |dom| control(dom, "Device controls").is_none()).await;
    assert_eq!(press(&mut dom, chord("b", false, true), Dom::default()).await.as_deref(), Some("rightPanel.toggle"));
    until(&mut dom, "the launcher", |dom| control(dom, "Open Device").is_some()).await;
    click_when_enabled(&mut dom, "Open Device").await;
    until(&mut dom, "it is running", |dom| control(dom, "Shut down Fixture iPhone").is_some() && control(dom, "Open Fixture iPhone").is_some()).await;
    click_control(&mut dom, "Shut down Fixture iPhone");
    until(&mut dom, "it is stopped", |dom| control(dom, "Start Fixture iPhone").is_some() && control(dom, "Shut down Fixture iPhone").is_none()).await;
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn setup_is_looked_at_but_not_changed_without_the_settings_grant() {
    let fixture = Fixture::start_with_devices(vec![]).await;
    MEMORY.with(|memory| memory.borrow_mut().clear());
    let (mut dom, props) = fixture.client(Gate::default(), &[AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate]);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    open_device_tab(&mut dom, &props, state).await;
    until(&mut dom, "the setup", |dom| control(dom, "Enable device hub").is_some()).await;
    assert_eq!(control_attribute(&dom, "Enable device hub", "disabled").and_then(|(_, value)| value).as_deref(), Some("true"));
    assert_eq!(service(&fixture).snapshot().host_status, DeviceHostStatus::Disabled);
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn the_integrations_page_turns_the_hub_and_agent_access_on_and_manages_the_hosts() {
    let fixture = Fixture::start_with_devices(vec![]).await;
    MEMORY.with(|memory| memory.borrow_mut().clear());
    let (mut dom, props) = fixture.first_launch(Gate::default(), &SCOPES);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some() && state.peek().config["settings"].is_object()).await;
    state.view().set(View::Integrations);
    until(&mut dom, "the page", |dom| control(dom, "Device hub").is_some() && control(dom, "Agent browser access").is_some()).await;
    let setting = |key: &str| state.peek().config["settings"][key].clone();
    assert_eq!((setting("enableDeviceSupport"), setting("enableAgentBrowserAccess")), (json!(false), json!(true)), "agents may use the browser unless it is turned off");
    assert!(control(&dom, "Agent device access").is_some() && rendered_text(&dom).contains("Device hosts"));
    assert_eq!(control_attribute(&dom, "Agent device access", "disabled").and_then(|(_, value)| value).as_deref(), Some("true"), "agents cannot have devices while the hub is off");

    // The browser switch is a setting of the environment.
    crate::runtime::transport_tests::change_control(&mut dom, "Agent browser access", "false");
    until(&mut dom, "the setting", |_| setting("enableAgentBrowserAccess") == json!(false)).await;

    // The hub starts on this machine; what each platform can do is said once it is up.
    crate::runtime::transport_tests::change_control(&mut dom, "Device hub", "true");
    until(&mut dom, "the hub", |dom| rendered_text(dom).contains("Simulator support") && service(&fixture).snapshot().host_status == DeviceHostStatus::Ready && control(dom, "Refresh simulator support").is_some()).await;
    assert_eq!(setting("enableDeviceSupport"), json!(true));
    until(&mut dom, "agent access enabled", |dom| control_attribute(dom, "Agent device access", "disabled").and_then(|(_, value)| value).as_deref() != Some("true")).await;
    crate::runtime::transport_tests::change_control(&mut dom, "Agent device access", "true");
    until(&mut dom, "agent access", |_| setting("enableAgentDeviceAccess") == json!(true)).await;
    assert!(service(&fixture).snapshot().agent_access_enabled);

    // A host is added after a test of its connection (this server has no SSH to try it with, and says so), and removed again.
    click_control(&mut dom, "Add device host");
    until(&mut dom, "the editor", |dom| control(dom, "Host name").is_some()).await;
    assert_eq!(control_attribute(&dom, "Save host", "disabled").and_then(|(_, value)| value).as_deref(), Some("true"), "a host needs a name and a target");
    crate::runtime::transport_tests::input_control(&mut dom, "Host name", "Mac mini");
    crate::runtime::transport_tests::input_control(&mut dom, "SSH target", "me@mini");
    until(&mut dom, "a valid draft", |dom| control_attribute(dom, "Test host connection", "disabled").and_then(|(_, value)| value).as_deref() != Some("true")).await;
    click_control(&mut dom, "Test host connection");
    until(&mut dom, "the failure", |dom| rendered_text(dom).contains("1 of 1 failed") && rendered_text(dom).contains("Show error")).await;
    // Once the host answers, its platforms are shown and the test passes.
    let probe = json!({"nodePath": "/usr/bin/node", "platforms": [{"platform": "ios", "available": true}, {"platform": "android", "available": false, "reason": "No SDK"}], "tools": {"hub": {"requiredVersion": t3_server::device_toolchain::DEVICE_HUB_VERSION, "installedVersions": [], "runningVersion": null}, "agent": {"requiredVersion": t3_server::device_toolchain::AGENT_DEVICE_VERSION, "installedVersions": [], "runningVersion": null}}});
    std::fs::write(fixture.device_files.join("ssh-reply"), probe.to_string()).unwrap();
    click_control(&mut dom, "Test host connection");
    until(&mut dom, "the pass", |dom| rendered_text(dom).contains("Connection checks passed") && rendered_text(dom).contains("iOS available") && rendered_text(dom).contains("Android unavailable")).await;
    click_when_enabled(&mut dom, "Save host").await;
    until(&mut dom, "the host", |dom| control(dom, "Edit Mac mini").is_some()).await;
    assert_eq!(setting("deviceHosts")[0]["target"], "me@mini");
    click_control(&mut dom, "Remove Mac mini");
    until(&mut dom, "no host", |dom| control(dom, "Edit Mac mini").is_none() && rendered_text(dom).contains("No device hosts.")).await;
    assert_eq!(setting("deviceHosts"), json!([]));

    // The size new browser tabs open at is this device's own preference.
    crate::runtime::transport_tests::change_control(&mut dom, "Default browser viewport", "freeform");
    until(&mut dom, "a fixed size", |_| matches!(state.peek().client_settings.browser_default_viewport, t3_contracts::PreviewViewportSetting::Freeform { .. })).await;
    until(&mut dom, "its size", |dom| control(dom, "Default viewport width").is_some()).await;
    fixture.finish(dom).await;
}
