//! The device panel as far as it is logic: what the picker lists and says (`DevicePanel.tsx`), the setup the first use asks
//! for (`DeviceSetup.tsx`), the hosts of the settings page (`deviceHostsSettings.logic.ts`, `deviceHostConnectionChecks.ts`)
//! and the hub addresses a device's picture and screenshot are read from (`hubAccess.ts`, `stream.ts`, `screenshot.ts`).
use serde_json::{Value, json};
use t3_contracts::{DeviceHostState, DeviceHostStatus, DevicePlatform, DeviceServiceState, DeviceSummary, Present};

pub fn state_of(value: &Value) -> Option<DeviceServiceState> {
    serde_json::from_value(value.clone()).ok()
}

pub fn device_key(host_id: &str, device_id: &str) -> String {
    format!("{host_id}\u{0}{device_id}")
}
pub fn platform_name(platform: DevicePlatform) -> &'static str {
    match platform {
        DevicePlatform::Ios => "iOS",
        DevicePlatform::Android => "Android",
    }
}
pub fn platform_wire(platform: DevicePlatform) -> &'static str {
    match platform {
        DevicePlatform::Ios => "ios",
        DevicePlatform::Android => "android",
    }
}
pub fn platform_group_label(platform: DevicePlatform) -> &'static str {
    match platform {
        DevicePlatform::Ios => "iOS Simulators",
        DevicePlatform::Android => "Android Emulators",
    }
}
pub fn platform_of(wire: &str) -> Option<DevicePlatform> {
    match wire {
        "ios" => Some(DevicePlatform::Ios),
        "android" => Some(DevicePlatform::Android),
        _ => None,
    }
}

/// `groupDevices`: the devices by platform, running ones first and then by name.
pub fn group_devices(state: &DeviceServiceState) -> Vec<(DevicePlatform, Vec<DeviceSummary>)> {
    let mut groups = Vec::new();
    for platform in [DevicePlatform::Ios, DevicePlatform::Android] {
        let mut devices: Vec<DeviceSummary> = state.devices.iter().filter(|device| device.platform == platform).cloned().collect();
        devices.sort_by(|a, b| b.booted.cmp(&a.booted).then_with(|| a.name.as_str().to_lowercase().cmp(&b.name.as_str().to_lowercase())));
        if !devices.is_empty() {
            groups.push((platform, devices));
        }
    }
    groups
}

pub fn host_label<'a>(state: &'a DeviceServiceState, host_id: &str) -> &'a str {
    state.hosts.iter().find(|host| host.id.as_str() == host_id).map_or("Device host", |host| host.label.as_str())
}

/// Whether any host can list devices now, and whether one is still getting there (and none is ready).
pub fn host_ready(state: &DeviceServiceState) -> bool {
    state.host_statuses.values().any(|host| host.status == DeviceHostStatus::Ready)
}
pub fn host_busy(state: &DeviceServiceState) -> bool {
    !host_ready(state) && state.host_statuses.values().any(|host| matches!(host.status, DeviceHostStatus::Installing | DeviceHostStatus::Starting))
}
pub fn unavailable_platforms(state: &DeviceServiceState) -> Vec<(String, DevicePlatform, Option<String>)> {
    state
        .hosts
        .iter()
        .flat_map(|host| host.platforms.iter().filter(|platform| !platform.available).map(|platform| (host.id.as_str().to_owned(), platform.platform, platform.reason.present().cloned())))
        .collect()
}
/// The sentence under the picker when it lists no device.
pub fn empty_picker_message(state: &DeviceServiceState) -> String {
    if state.host_status == DeviceHostStatus::Failed {
        state.host_status_detail.present().cloned().unwrap_or_else(|| "The device hub failed to start.".into())
    } else {
        "No simulators or emulators were found on this environment.".into()
    }
}
/// The line under the picker when devices are listed but no Android virtual device is, and nothing says why.
pub fn needs_android_hint(state: &DeviceServiceState) -> bool {
    host_ready(state) && !state.devices.iter().any(|device| device.platform == DevicePlatform::Android) && !unavailable_platforms(state).iter().any(|(_, platform, _)| *platform == DevicePlatform::Android)
}
/// What the picker shows in place of its list while it waits: a device being opened, a host getting ready, or the first read.
pub fn loading_message(state: &DeviceServiceState, opening: Option<&DeviceSummary>) -> String {
    match opening {
        Some(device) if device.booted => "Opening device…".into(),
        Some(_) => "Starting device…".into(),
        None if state.host_status == DeviceHostStatus::Installing => state.host_status_detail.present().cloned().unwrap_or_else(|| "Installing device support…".into()),
        None => "Finding devices…".into(),
    }
}
/// The row of a device in the picker: where it runs, its version and whether it is running.
pub fn device_description(state: &DeviceServiceState, device: &DeviceSummary) -> String {
    format!("{} · {} · {}", host_label(state, device.host_id.as_str()), device.version, if device.booted { "Running" } else { "Stopped" })
}

/// `platformSetupStatus`: whether a platform can be used, and the sentence that says so or says what is missing.
pub fn platform_setup_status(state: &DeviceServiceState, platform: DevicePlatform) -> (bool, String) {
    let availability = state.hosts.iter().flat_map(|host| host.platforms.iter()).find(|candidate| candidate.platform == platform);
    let Some(availability) = availability.filter(|availability| availability.available) else {
        return (false, availability.and_then(|availability| availability.reason.present().cloned()).unwrap_or_else(|| format!("{} support was not detected.", platform_name(platform))));
    };
    let _ = availability;
    if state.host_status == DeviceHostStatus::Ready && !state.devices.iter().any(|device| device.platform == platform) {
        return (
            false,
            match platform {
                DevicePlatform::Ios => "Xcode is installed, but no iOS Simulator is available. Install a runtime in Xcode Settings → Components.",
                DevicePlatform::Android => "The Android SDK is installed, but no virtual device exists. Create one in Android Studio → Device Manager.",
            }
            .into(),
        );
    }
    (
        true,
        match platform {
            DevicePlatform::Ios => "Xcode and iOS Simulator are available.",
            DevicePlatform::Android => "The Android SDK and Emulator are available.",
        }
        .into(),
    )
}
pub const DEVICE_HUB_DESCRIPTION: &str = "Enable this environment to open simulators and emulators, whether they run here or on a remote device host.";
pub const AGENT_DEVICE_DESCRIPTION: &str = "Allow new agent sessions in this environment to start and control local and remote devices, with required tools set up automatically.";

/// `DeviceHubSetupStatus` while it is busy: what the hub is doing.
pub fn hub_progress(status: DeviceHostStatus, compact: bool) -> &'static str {
    match (status, compact) {
        (DeviceHostStatus::Installing, true) => "Installing…",
        (DeviceHostStatus::Installing, false) => "Installing device hub…",
        (DeviceHostStatus::Starting, true) => "Starting…",
        (DeviceHostStatus::Starting, false) => "Starting device hub…",
        (_, true) => "Updating…",
        (_, false) => "Updating device hub…",
    }
}
/// `AgentDeviceSetupStatus` while it is busy.
pub fn agent_progress(status: DeviceHostStatus, compact: bool) -> &'static str {
    match (status, compact) {
        (DeviceHostStatus::Installing, true) => "Installing…",
        (DeviceHostStatus::Installing, false) => "Installing agent tools…",
        (DeviceHostStatus::Starting, true) => "Starting…",
        (DeviceHostStatus::Starting, false) => "Starting agent tools…",
        (_, true) => "Updating…",
        (_, false) => "Updating agent access…",
    }
}
/// Whether the agent's tools are installed and on (the line "Agent tools are ready.").
pub fn agent_tools_ready(state: &DeviceServiceState) -> bool {
    state.agent_access_enabled && state.host_status == DeviceHostStatus::Ready && state.hosts.iter().any(|host| host.agent_device_installed)
}
/// The setup wizard stands in for the panel until the hub is on and the first run is done.
pub fn setup_needed(loaded: bool, state: &DeviceServiceState) -> bool {
    loaded && (!state.onboarding_completed || state.host_status == DeviceHostStatus::Disabled)
}

/// What a host of the settings page says it is doing (`DeviceHostList`): the progress line while it connects, or its error.
pub fn host_progress(status: Option<&DeviceHostState>, checking: bool) -> Option<&'static str> {
    if checking {
        return Some("Checking connection…");
    }
    match status.map(|status| status.status) {
        Some(DeviceHostStatus::Installing) => Some("Installing device support…"),
        Some(DeviceHostStatus::Starting) => Some("Connecting…"),
        _ => None,
    }
}
pub fn host_error(status: Option<&DeviceHostState>) -> Option<String> {
    status.filter(|status| status.status == DeviceHostStatus::Failed).and_then(|status| status.detail.present().cloned())
}
/// Whether a failed host offers Retry instead of Test connection.
pub fn can_retry(state: &DeviceServiceState, status: Option<&DeviceHostState>) -> bool {
    status.is_some_and(|status| status.status == DeviceHostStatus::Failed) && state.supports_host_retry.present() == Some(&true) && state.host_status != DeviceHostStatus::Disabled
}

// ---- the hosts of the settings page ------------------------------------------------------------------------------------

/// `parseDeviceHostDraft`: the host a form describes, or nothing when the server would refuse it. An identity file that is
/// only blank is left out.
pub fn parse_host_draft(id: &str, label: &str, target: &str, port: Option<u64>, identity_file: &str) -> Option<Value> {
    let mut host = json!({"id": id, "label": label, "target": target});
    if let Some(port) = port {
        host["port"] = json!(port);
    }
    if !identity_file.trim().is_empty() {
        host["identityFile"] = json!(identity_file.trim());
    }
    serde_json::from_value::<t3_contracts::SshDeviceHostConfig>(host.clone()).ok().map(|_| host)
}
/// `deviceHostConnectionKey`: two hosts that reach the same machine the same way are one connection to test.
pub fn connection_key(host: &Value) -> String {
    json!([host["target"].as_str().unwrap_or_default().trim(), host["port"], host["identityFile"].as_str().map(str::trim).filter(|file| !file.is_empty())]).to_string()
}
fn same_destination(a: &Value, b: &Value) -> bool {
    a["target"] == b["target"] && a["port"] == b["port"] && a["identityFile"] == b["identityFile"]
}
/// `updateDeviceHosts`: one host added, changed or removed in an environment's list without touching the others. A host is found by
/// its id, else by where it connects, so a save that is repeated after a partial failure lands on the host it already changed.
pub fn update_device_hosts(hosts: &[Value], host: &Value, remove: bool, original: &Value) -> Result<Vec<Value>, String> {
    let find_destination = |destination: &Value| -> Result<Option<usize>, String> {
        let matches: Vec<usize> = hosts.iter().enumerate().filter(|(_, candidate)| same_destination(candidate, destination)).map(|(index, _)| index).collect();
        if matches.len() > 1 {
            return Err("Multiple hosts match this SSH destination. Select the environment to edit its hosts.".into());
        }
        Ok(matches.first().copied())
    };
    let existing = match hosts.iter().position(|candidate| candidate["id"] == original["id"]) {
        Some(index) => Some(index),
        None => match find_destination(original)? {
            Some(index) => Some(index),
            None if remove => None,
            None => find_destination(host)?,
        },
    };
    let mut next = hosts.to_vec();
    if remove {
        return Ok(hosts.iter().enumerate().filter(|(index, _)| Some(*index) != existing).map(|(_, host)| host.clone()).collect());
    }
    match existing {
        Some(index) => {
            let mut changed = host.clone();
            changed["id"] = hosts[index]["id"].clone();
            next[index] = changed;
        }
        None => next.push(host.clone()),
    }
    Ok(next)
}

/// What one environment answered to "can you reach this host?" (`DeviceHostCheck`).
#[derive(Debug, Clone, PartialEq)]
pub enum Check {
    Pending,
    /// The host is the machine the environment runs on.
    Local,
    Connected { platforms: Vec<(DevicePlatform, bool)> },
    Failed(String),
}
/// The answer of `device.testHost`: a summary of the host, which for the machine itself says so.
pub fn check_of(summary: &Value) -> Check {
    if summary["kind"] == "local" {
        return Check::Local;
    }
    let platforms = summary["platforms"].as_array().into_iter().flatten().filter_map(|platform| Some((platform_of(platform["platform"].as_str()?)?, platform["available"].as_bool()?))).collect();
    Check::Connected { platforms }
}

// ---- the hub ----------------------------------------------------------------------------------------------------------

/// Where a device's own picture and controls are served (`vendorBase`): the hub proxies serve-sim for iOS and serve-emu for Android.
pub fn vendor_base(hub_base_path: &str, platform: DevicePlatform) -> String {
    format!("{}/vendor/{}", hub_base_path.trim_end_matches('/'), if platform == DevicePlatform::Ios { "serve-sim" } else { "serve-emu" })
}
/// The hub address of one host, relative to the environment (`?hostId=` names the host; the hub serves the local one without it).
pub fn hub_path(hub_base_path: &str, host_id: &str, path: &str, params: &[(&str, &str)]) -> String {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    for (name, value) in params {
        query.append_pair(name, value);
    }
    query.append_pair("hostId", host_id);
    format!("{}{}?{}", hub_base_path.trim_end_matches('/'), path, query.finish())
}
/// The live picture: iOS's MJPEG stream, which an `<img>` can show with the ticket in its address.
pub fn mjpeg_path(hub_base_path: &str, host_id: &str, device_id: &str) -> String {
    hub_path(&vendor_base(hub_base_path, DevicePlatform::Ios), host_id, &format!("/helper/{}/stream.mjpeg", url::form_urlencoded::byte_serialize(device_id.as_bytes()).collect::<String>()), &[])
}
/// `captureDeviceScreenshot`: a POST that answers a PNG.
pub fn screenshot_path(hub_base_path: &str, host_id: &str, platform: DevicePlatform, device_id: &str) -> String {
    hub_path(&vendor_base(hub_base_path, platform), host_id, "/api/screenshot", &[("device", device_id)])
}
/// The file a screenshot is saved as: the device's name made safe, and the time.
pub fn screenshot_file_name(device_name: &str, now_ms: i64) -> String {
    let safe: String = device_name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' }).collect();
    format!("{safe}-{now_ms}.png")
}

/// The `device.action` the quick controls make (`useDeviceControls`' `act`): the target is the device's.
pub fn action_input(host_id: &str, device_id: &str, action: Value) -> Value {
    let mut input = action;
    input["hostId"] = json!(host_id);
    input["deviceId"] = json!(device_id);
    input
}
/// The appearance a click on the rail's sun/moon button asks for.
pub fn next_appearance(current: Option<&str>) -> &'static str {
    if current == Some("dark") { "light" } else { "dark" }
}
pub const TEXT_SIZES: [(&str, &str); 4] = [("small", "Small"), ("default", "Default"), ("large", "Large"), ("extra-large", "Extra large")];

#[cfg(test)]
mod tests {
    use super::*;

    fn state(value: Value) -> DeviceServiceState {
        let mut base = json!({
            "hosts": [{"id": "local", "kind": "local", "label": "This Mac", "platforms": [{"platform": "ios", "available": true}, {"platform": "android", "available": false, "reason": "No Android SDK"}], "hubInstalled": true, "agentDeviceInstalled": false}],
            "hostStatus": "ready", "hostStatuses": {"local": {"status": "ready"}}, "devices": [], "sessions": [], "onboardingCompleted": true,
            "agentAccessEnabled": false, "hubBasePath": "/api/device-hub", "revision": 1
        });
        for (key, value) in value.as_object().into_iter().flatten() {
            base[key] = value.clone();
        }
        serde_json::from_value(base).unwrap()
    }
    fn device(id: &str, name: &str, platform: &str, booted: bool) -> Value {
        json!({"hostId": "local", "id": id, "platform": platform, "name": name, "version": "18.0", "booted": booted, "physical": false})
    }

    #[test]
    fn devices_group_by_platform_running_first_then_by_name() {
        let state = state(json!({"devices": [device("a", "iPhone SE", "ios", false), device("b", "iPhone 16", "ios", false), device("c", "iPad", "ios", true), device("d", "Pixel", "android", false)]}));
        let groups = group_devices(&state);
        assert_eq!(groups.iter().map(|(platform, _)| platform_group_label(*platform)).collect::<Vec<_>>(), ["iOS Simulators", "Android Emulators"]);
        assert_eq!(groups[0].1.iter().map(|device| device.name.as_str()).collect::<Vec<_>>(), ["iPad", "iPhone 16", "iPhone SE"]);
        assert_eq!(device_description(&state, &groups[0].1[0]), "This Mac · 18.0 · Running");
        assert_eq!(device_key("local", "c"), "local\u{0}c");
    }

    #[test]
    fn the_picker_says_what_it_is_waiting_for_and_what_is_missing() {
        let starting = state(json!({"hostStatus": "starting", "hostStatuses": {"local": {"status": "starting"}}}));
        assert!(host_busy(&starting) && !host_ready(&starting));
        assert_eq!(loading_message(&starting, None), "Finding devices…");
        let installing = state(json!({"hostStatus": "installing", "hostStatuses": {"local": {"status": "installing"}}, "hostStatusDetail": "Downloading the hub (40%)"}));
        assert_eq!(loading_message(&installing, None), "Downloading the hub (40%)");
        let ready = state(json!({}));
        assert!(!host_busy(&ready) && needs_android_hint(&state(json!({"hosts": [{"id": "local", "kind": "local", "label": "This Mac", "platforms": [{"platform": "ios", "available": true}], "hubInstalled": true, "agentDeviceInstalled": false}]}))));
        assert!(!needs_android_hint(&ready), "an unavailable Android already says why");
        assert_eq!(unavailable_platforms(&ready), vec![("local".to_owned(), DevicePlatform::Android, Some("No Android SDK".to_owned()))]);
        let booted = device_summary(&ready);
        assert_eq!(loading_message(&ready, Some(&booted)), "Starting device…");
        let failed = state(json!({"hostStatus": "failed", "hostStatusDetail": "The hub crashed"}));
        assert_eq!(empty_picker_message(&failed), "The hub crashed");
        assert_eq!(empty_picker_message(&ready), "No simulators or emulators were found on this environment.");
    }
    fn device_summary(_: &DeviceServiceState) -> DeviceSummary {
        serde_json::from_value(device("x", "Phone", "ios", false)).unwrap()
    }

    #[test]
    fn platform_setup_reads_availability_then_whether_a_device_exists() {
        let ready = state(json!({}));
        assert_eq!(platform_setup_status(&ready, DevicePlatform::Android), (false, "No Android SDK".into()));
        let (ok, message) = platform_setup_status(&ready, DevicePlatform::Ios);
        assert!(!ok && message.starts_with("Xcode is installed, but no iOS Simulator"));
        let with_phone = state(json!({"devices": [device("a", "iPhone", "ios", true)]}));
        assert_eq!(platform_setup_status(&with_phone, DevicePlatform::Ios), (true, "Xcode and iOS Simulator are available.".into()));
        let unknown = state(json!({"hosts": []}));
        assert_eq!(platform_setup_status(&unknown, DevicePlatform::Ios), (false, "iOS support was not detected.".into()));
        assert!(!setup_needed(false, &unknown));
        assert!(setup_needed(true, &state(json!({"onboardingCompleted": false}))));
        assert!(setup_needed(true, &state(json!({"hostStatus": "disabled"}))));
        assert!(!setup_needed(true, &ready));
        assert_eq!(hub_progress(DeviceHostStatus::Starting, false), "Starting device hub…");
        assert_eq!(agent_progress(DeviceHostStatus::Idle, true), "Updating…");
    }

    #[test]
    fn a_failed_host_offers_retry_only_when_the_server_can_and_the_hub_is_on() {
        let failed = state(json!({"supportsHostRetry": true, "hostStatuses": {"ssh1": {"status": "failed", "detail": "ssh: refused"}}}));
        let status = failed.host_statuses.values().next();
        assert!(can_retry(&failed, status));
        assert_eq!(host_error(status).as_deref(), Some("ssh: refused"));
        assert_eq!(host_progress(status, false), None);
        assert_eq!(host_progress(status, true), Some("Checking connection…"));
        assert!(!can_retry(&state(json!({"hostStatuses": {"ssh1": {"status": "failed"}}})), failed.host_statuses.values().next()), "an older server cannot retry");
        assert!(!can_retry(&state(json!({"supportsHostRetry": true, "hostStatus": "disabled", "hostStatuses": {"ssh1": {"status": "failed"}}})), failed.host_statuses.values().next()));
    }

    #[test]
    fn a_host_draft_is_valid_when_the_server_would_take_it() {
        assert_eq!(parse_host_draft("h1", "Mac mini", "me@mini", Some(2222), "  ~/.ssh/key "), Some(json!({"id": "h1", "label": "Mac mini", "target": "me@mini", "port": 2222, "identityFile": "~/.ssh/key"})));
        assert_eq!(parse_host_draft("h1", "Mac mini", "me@mini", None, "   "), Some(json!({"id": "h1", "label": "Mac mini", "target": "me@mini"})));
        assert_eq!(parse_host_draft("h1", "Mac mini", "me mini", None, ""), None, "a target has no whitespace");
        assert_eq!(parse_host_draft("h1", "Mac mini", "-oProxyCommand=x", None, ""), None, "nor a leading dash");
        assert_eq!(parse_host_draft("h1", "Mac mini", "mini", Some(70_000), ""), None);
        assert_eq!(parse_host_draft("local", "Mac mini", "mini", None, ""), None, "local is the machine itself");
    }

    #[test]
    fn saving_a_host_changes_that_host_and_finds_it_again_by_where_it_connects() {
        let a = json!({"id": "a", "label": "A", "target": "a@a"});
        let b = json!({"id": "b", "label": "B", "target": "b@b", "port": 22});
        let hosts = vec![a.clone(), b.clone()];
        // A new host is added; an edited one keeps the id the environment gave it.
        let c = json!({"id": "c", "label": "C", "target": "c@c"});
        assert_eq!(update_device_hosts(&hosts, &c, false, &c).unwrap().len(), 3);
        let edited = json!({"id": "other", "label": "A renamed", "target": "a@a"});
        let saved = update_device_hosts(&hosts, &edited, false, &a).unwrap();
        assert_eq!(saved[0], json!({"id": "a", "label": "A renamed", "target": "a@a"}));
        // Another environment holds the same host under another id: found by its destination.
        let elsewhere = vec![json!({"id": "zzz", "label": "A", "target": "a@a"})];
        assert_eq!(update_device_hosts(&elsewhere, &edited, false, &a).unwrap(), vec![json!({"id": "zzz", "label": "A renamed", "target": "a@a"})]);
        assert_eq!(update_device_hosts(&elsewhere, &a, true, &a).unwrap(), Vec::<Value>::new());
        assert_eq!(update_device_hosts(&hosts, &a, true, &a).unwrap(), vec![b.clone()]);
        assert_eq!(update_device_hosts(&hosts, &c, true, &c).unwrap(), hosts, "removing a host that is not there removes nothing");
        // A destination two hosts share is not guessed at.
        let twins = vec![json!({"id": "1", "label": "A", "target": "t"}), json!({"id": "2", "label": "B", "target": "t"})];
        assert!(update_device_hosts(&twins, &json!({"id": "9", "label": "T", "target": "t"}), false, &json!({"id": "9", "label": "T", "target": "t"})).is_err());
        assert_eq!(connection_key(&json!({"target": " a@a ", "port": 22})), r#"["a@a",22,null]"#);
    }

    #[test]
    fn a_host_test_that_lands_on_this_machine_says_so() {
        assert_eq!(check_of(&json!({"kind": "local"})), Check::Local);
        assert_eq!(check_of(&json!({"kind": "ssh", "platforms": [{"platform": "ios", "available": true}, {"platform": "android", "available": false}]})), Check::Connected { platforms: vec![(DevicePlatform::Ios, true), (DevicePlatform::Android, false)] });
    }

    #[test]
    fn hub_addresses_name_the_host_and_the_vendor() {
        assert_eq!(vendor_base("/api/device-hub", DevicePlatform::Ios), "/api/device-hub/vendor/serve-sim");
        assert_eq!(mjpeg_path("/api/device-hub/", "local", "AB-12"), "/api/device-hub/vendor/serve-sim/helper/AB-12/stream.mjpeg?hostId=local");
        assert_eq!(screenshot_path("/api/device-hub", "ssh1", DevicePlatform::Android, "emulator-5554"), "/api/device-hub/vendor/serve-emu/api/screenshot?device=emulator-5554&hostId=ssh1");
        assert_eq!(screenshot_file_name("iPhone 16 Pro", 5), "iPhone-16-Pro-5.png");
        assert_eq!(action_input("local", "d", json!({"type": "setAppearance", "value": "dark"})), json!({"type": "setAppearance", "value": "dark", "hostId": "local", "deviceId": "d"}));
        assert_eq!((next_appearance(Some("dark")), next_appearance(None)), ("light", "dark"));
    }
}
