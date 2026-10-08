//! Device toolkit handlers. Every entry uses the trusted invocation access gate.
use crate::{
    device_service::DeviceService,
    mcp_invocation::{InvocationScope, McpCapability, McpFailure},
    persistence::Store,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::path::PathBuf;
use t3_contracts::*;

fn unavailable(reason: impl Into<String>) -> McpFailure {
    McpFailure(json!({"_tag":"DeviceToolUnavailableError","reason":reason.into()}))
}
fn device_error(error: DeviceError) -> McpFailure {
    McpFailure(serde_json::to_value(error).unwrap())
}
pub(crate) fn failure_text(error: &McpFailure) -> String {
    let fields = &error.0;
    let string = |key: &str| fields[key].as_str().unwrap_or("");
    match string("_tag") {
        "DeviceToolUnavailableError" => string("reason").into(),
        "DeviceHostUnavailableError" => format!(
            "Device host {} is unavailable: {}",
            string("hostId"),
            string("reason")
        ),
        "DevicePlatformUnavailableError" => format!(
            "{} devices are unavailable on host {}: {}",
            string("platform"),
            string("hostId"),
            string("reason")
        ),
        "DeviceNotFoundError" => format!(
            "Device {} was not found on host {}.",
            string("deviceId"),
            string("hostId")
        ),
        "DeviceBootError" => format!(
            "Device {} failed to boot: {}",
            string("deviceId"),
            match string("reason") {
                "disk_space" => "There is not enough free disk space on the environment server.",
                "timeout" => "The device did not become ready in time.",
                _ =>
                    "The simulator or emulator could not start. Check its configuration on the environment server.",
            }
        ),
        "DeviceOperationError" => {
            let explanation = match string("reason") {
                "command_failed" => format!(
                    "The device command failed{}.",
                    fields
                        .get("exitCode")
                        .map(|code| format!(
                            " (exit code {})",
                            crate::device_actions::stringify(code)
                        ))
                        .unwrap_or_default()
                ),
                "request_failed" => {
                    "Could not communicate with device support. Try refreshing devices.".into()
                }
                "invalid_payload" => "The device request could not be encoded.".into(),
                "settings_failed" => "Could not read or save device settings.".into(),
                _ => "The device hub could not complete the request.".into(),
            };
            format!("Device {} failed: {explanation}", string("operation"))
        }
        "DeviceActionUnavailableError" => {
            if string("reason") == "helper_missing" {
                format!(
                    "Device {} requires a helper missing from this install. Set up device support again.",
                    string("operation")
                )
            } else {
                format!(
                    "Device {} is not supported on {}.",
                    string("operation"),
                    string("platform")
                )
            }
        }
        _ => error.to_string(),
    }
}
fn access(scope: &InvocationScope) -> Result<&crate::mcp_invocation::ThreadCaller, McpFailure> {
    scope
        .require_capability(McpCapability::Device, true)
        .map_err(|_| unavailable("Agent device access is turned off for this environment."))?;
    scope.require_thread("This tool")
}
fn enabled(state: &DeviceServiceState) -> Result<(), McpFailure> {
    if state.host_status == DeviceHostStatus::Disabled {
        Err(unavailable(
            "Device support is off. Ask the user to enable it in the Device panel before installing or starting device tools.",
        ))
    } else {
        Ok(())
    }
}
fn local() -> DeviceHostId {
    DeviceHostId::new("local").unwrap()
}
fn value<T>(field: Option<Option<T>>) -> Option<T> {
    field.flatten()
}

pub fn pick_device(
    devices: &[DeviceSummary],
    input: &DeviceToolOpenInput,
) -> Result<DeviceSummary, McpFailure> {
    let host = input
        .host_id
        .as_ref()
        .and_then(Option::as_ref)
        .cloned()
        .unwrap_or_else(local);
    let device_id = input.device_id.as_ref().and_then(Option::as_ref);
    let platform = input.platform.as_ref().and_then(Option::as_ref);
    if let Some(id) = device_id {
        return devices
            .iter()
            .find(|device| device.host_id == host && &device.id == id)
            .cloned()
            .ok_or_else(|| {
                unavailable(format!(
                    "No device {id} on host {host}. Call device_list for current ids."
                ))
            });
    }
    let candidates: Vec<_> = devices
        .iter()
        .filter(|device| {
            device.host_id == host && platform.is_none_or(|platform| &device.platform == platform)
        })
        .collect();
    if candidates.is_empty() {
        return Err(unavailable(match platform {
            None => "No simulators or emulators were found. Call device_list to see why.".into(),
            Some(platform) => format!(
                "No {} devices were found on host {host}. Call device_list to see why.",
                serde_json::to_value(platform).unwrap().as_str().unwrap()
            ),
        }));
    }
    if platform.is_none()
        && candidates
            .iter()
            .any(|device| device.platform != candidates[0].platform)
    {
        return Err(unavailable(
            "Both iOS and Android devices are available; pass platform or deviceId.",
        ));
    }
    Ok((*candidates
        .iter()
        .find(|device| device.booted)
        .unwrap_or(&candidates[0]))
    .clone())
}
pub fn target_args(device: &DeviceSummary) -> Vec<String> {
    vec![
        "--platform".into(),
        if device.platform == DevicePlatform::Ios {
            "ios"
        } else {
            "android"
        }
        .into(),
        if device.platform == DevicePlatform::Ios {
            "--udid"
        } else {
            "--serial"
        }
        .into(),
        device.id.to_string(),
    ]
}
fn quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "_./:-".contains(character))
    {
        value.to_owned()
    } else {
        crate::agent_device_launcher::shell_quote(value)
    }
}
pub fn quick_start(device: &DeviceSummary, args: &[String], command: &str) -> String {
    let executable = quote(command);
    let target = args
        .iter()
        .map(|arg| quote(arg))
        .collect::<Vec<_>>()
        .join(" ");
    let platform_notes = if device.platform == DevicePlatform::Ios {
        "First use builds an XCTest runner and can take a couple of minutes; later commands are fast."
    } else {
        "The Android snapshot helper installs itself on first use."
    };
    [format!("The user is watching {} ({}) in the Device panel.",device.name,device.version),
     format!("Drive it with {executable}. Use this exact executable path; login shells may reset PATH. Always pass {target}."),
     "Typical loop:".into(),
     format!("  {executable} open <bundle-or-package-id> {target}     # or: open <app> <deep-link-url>"),
     format!("  {executable} snapshot -i {target}                     # accessibility tree with @eN refs"),
     format!("  {executable} click @e3 {target}"),
     format!("  {executable} fill @e5 \"text\" {target}"),
     format!("  {executable} screenshot /tmp/shot.png {target}        # or call device_screenshot"),
     format!("  {executable} install <app> <path-to-.app-or-.apk> {target}"),
     format!("Prefer snapshot refs over coordinates. Run {executable} help for workflow guides and {executable} <command> --help for flags."),
     "Prefer agent-device for driving this device. simctl, adb, and xcrun remain available for anything it does not cover.".into(),
     "For remote hosts, arrange builds, app installation, and any Metro reverse forwarding yourself. T3 provides discovery, streaming, and control only.".into(),
     "Keep the returned --config and --session flags on every command. Other hosts can be used concurrently; opening one does not switch these commands.".into(),
     platform_notes.into()].join("\n")
}
pub fn png_dimensions(png: &[u8]) -> (u32, u32) {
    if png.len() < 24
        || png[..8] != [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]
        || png[12..16] != *b"IHDR"
    {
        return (0, 0);
    }
    (
        u32::from_be_bytes(png[16..20].try_into().unwrap()),
        u32::from_be_bytes(png[20..24].try_into().unwrap()),
    )
}
#[derive(Clone)]
pub struct McpDeviceTools {
    pub devices: DeviceService,
    pub store: Store,
    pub state_dir: PathBuf,
    pub executable: PathBuf,
}
impl McpDeviceTools {
    pub async fn list(
        &self,
        scope: &InvocationScope,
        host: Option<&str>,
    ) -> Result<DeviceToolListResult, McpFailure> {
        scope.reads_as_caller()?;
        let caller = access(scope)?;
        let state = self.devices.list().await.map_err(device_error)?;
        enabled(&state)?;
        let host = host.filter(|host| !host.is_empty());
        Ok(DeviceToolListResult {
            host_statuses: state
                .host_statuses
                .into_iter()
                .filter(|(id, _)| host.is_none_or(|host| id.as_str() == host))
                .collect(),
            hosts: state
                .hosts
                .into_iter()
                .filter(|item| host.is_none_or(|host| item.id.as_str() == host))
                .collect(),
            devices: state
                .devices
                .into_iter()
                .filter(|item| host.is_none_or(|host| item.host_id.as_str() == host))
                .collect(),
            open: state
                .sessions
                .into_iter()
                .filter(|session| session.thread_id == caller.thread_id)
                .map(|session| DeviceToolOpenTarget {
                    host_id: session.host_id,
                    device_id: session.device_id,
                })
                .collect(),
        })
    }
    pub async fn open(
        &self,
        scope: &InvocationScope,
        input: DeviceToolOpenInput,
    ) -> Result<DeviceToolOpenResult, McpFailure> {
        scope.acts_as_caller(&self.store)?;
        let caller = access(scope)?;
        let state = self.devices.list().await.map_err(device_error)?;
        enabled(&state)?;
        let target = pick_device(&state.devices, &input)?;
        // Consent and an authenticated agent endpoint precede boot/session registration.
        let mut agent_args = self
            .devices
            .agent_target(&caller.thread_id, &target.host_id, &target.id)
            .await
            .map_err(device_error)?;
        let session = self
            .devices
            .open(DeviceOpenInput {
                thread_id: caller.thread_id.clone(),
                host_id: Some(Some(target.host_id.clone())),
                device_id: target.id.clone(),
                platform: target.platform,
                boot: None,
            })
            .await
            .map_err(device_error)?;
        let device = self
            .devices
            .snapshot()
            .devices
            .into_iter()
            .find(|device| device.host_id == session.host_id && device.id == session.device_id)
            .unwrap_or(target);
        let mut args = target_args(&device);
        args.append(&mut agent_args);
        let (node, entry) = self.devices.agent_cli().await.map_err(device_error)?;
        let state_dir = self.state_dir.clone();
        let executable = self.executable.clone();
        let command = tokio::task::spawn_blocking(move || crate::agent_device_launcher::ensure(&state_dir,&executable,&node,&entry)).await
            .map_err(|_|unavailable("Could not prepare the agent-device launcher."))?
            .map_err(|error|McpFailure(json!({"_tag":"DeviceToolUnavailableError","reason":"Could not prepare the agent-device launcher.","cause":error.to_string()})))?.to_string_lossy().into_owned();
        Ok(DeviceToolOpenResult {
            quick_start: quick_start(&device, &args, &command),
            device,
            agent_device: DeviceAgentInvocation {
                command,
                target_args: args,
            },
        })
    }
    pub async fn close(
        &self,
        scope: &InvocationScope,
        input: DeviceToolCloseInput,
    ) -> Result<Value, McpFailure> {
        scope.acts_as_caller(&self.store)?;
        let caller = access(scope)?;
        self.devices
            .close(DeviceCloseInput {
                thread_id: caller.thread_id.clone(),
                host_id: input.host_id,
                device_id: input.device_id,
                shutdown: input.shutdown,
            })
            .await
            .map_err(device_error)?;
        Ok(json!({}))
    }
    pub async fn screenshot(
        &self,
        scope: &InvocationScope,
        input: DeviceToolTargetInput,
    ) -> Result<DeviceToolScreenshotResult, McpFailure> {
        scope.reads_as_caller()?;
        let caller = access(scope)?;
        let host = value(input.host_id);
        let target = if let Some(device) = value(input.device_id) {
            (host.unwrap_or_else(local), device)
        } else {
            let session = self
                .devices
                .sessions_for_thread(&caller.thread_id)
                .into_iter()
                .filter(|session| host.as_ref().is_none_or(|host| &session.host_id == host))
                .last()
                .ok_or_else(|| {
                    unavailable("No device is open in this thread. Call device_open first.")
                })?;
            (session.host_id, session.device_id)
        };
        let (device, png) = self
            .devices
            .screenshot(Some(&target.0), &target.1)
            .await
            .map_err(device_error)?;
        let (width, height) = png_dimensions(&png);
        Ok(DeviceToolScreenshotResult {
            device,
            screenshot: DeviceScreenshotData {
                mime_type: DevicePngMime::Png,
                data: STANDARD.encode(png),
                width: SafeInt(width as i64),
                height: SafeInt(height as i64),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_device_selection_guidance_and_png_oracle() {
        let mut count = 0;
        for line in include_str!("../tests/fixtures/mcp-device.jsonl")
            .split('\n')
            .filter(|line| !line.is_empty())
        {
            let row: Value = serde_json::from_str(line).unwrap();
            match row["type"].as_str().unwrap() {
                "guidance" => {
                    let device: DeviceSummary =
                        serde_json::from_value(row["device"].clone()).unwrap();
                    let args: Vec<String> = serde_json::from_value(row["args"].clone()).unwrap();
                    assert_eq!(json!(target_args(&device)), row["targetArgs"]);
                    assert_eq!(
                        json!(quick_start(
                            &device,
                            &args,
                            row["command"].as_str().unwrap()
                        )),
                        row["result"]
                    );
                }
                "pick" => {
                    let devices: Vec<DeviceSummary> =
                        serde_json::from_value(row["devices"].clone()).unwrap();
                    let input = serde_json::from_value(row["input"].clone()).unwrap();
                    let result = match pick_device(&devices, &input) {
                        Ok(device) => json!({"device":device}),
                        Err(error) => json!({"error":error.0}),
                    };
                    assert_eq!(result, row["result"]);
                }
                "png" => {
                    let bytes: Vec<u8> = serde_json::from_value(row["bytes"].clone()).unwrap();
                    let (width, height) = png_dimensions(&bytes);
                    assert_eq!(json!({"width":width,"height":height}), row["result"]);
                }
                _ => panic!("unexpected source oracle"),
            }
            count += 1;
        }
        assert_eq!(count, 144);
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn actual_device_handlers_guard_before_start_pin_target_and_keep_thread_sessions_scoped()
    {
        tokio::time::timeout(std::time::Duration::from_secs(20),async {
            let root=tempfile::tempdir().unwrap();let fixture=crate::device_service::tests::fixture(root.path()).await;
            let(store,scope)=crate::mcp_invocation::tests::fixture();
            let tools=McpDeviceTools{devices:fixture.service.clone(),store:store.clone(),state_dir:root.path().join("launcher-state"),executable:std::env::current_exe().unwrap()};
            let mut outside=scope.clone();outside.thread=None;
            assert_eq!(tools.list(&outside,None).await.unwrap_err().0["code"],"thread_credential_required");
            assert_eq!(tools.open(&scope,serde_json::from_value(json!({})).unwrap()).await.unwrap_err().0["code"],"parent_not_active");
            let mut denied=scope.clone();denied.capabilities.clear();
            assert_eq!(tools.list(&denied,None).await.unwrap_err().0["reason"],"Agent device access is turned off for this environment.");
            assert!(fixture.service.current_readiness(None).is_none());
            assert_eq!(tools.list(&scope,None).await.unwrap_err().0["reason"],"Device support is off. Ask the user to enable it in the Device panel before installing or starting device tools.");
            fixture.service.configure(serde_json::from_value(json!({"enabled":true})).unwrap()).await.unwrap();
            let projection=store.projection("thread",scope.thread.as_ref().unwrap().thread_id.as_str()).unwrap().unwrap();
            let mut projection=projection;projection["runs"]=json!([{ "id":"run:1", "status":"running"}]);
            store.transaction(|tx|crate::persistence::write_projection(tx,"thread",scope.thread.as_ref().unwrap().thread_id.as_str(),&projection)).unwrap();
            let input=||serde_json::from_value(json!({"deviceId":"fixture-ios"})).unwrap();
            // Consent is resolved before booting/registering a session.
            assert!(tools.open(&scope,input()).await.is_err());assert!(fixture.service.sessions_for_thread(&scope.thread.as_ref().unwrap().thread_id).is_empty());
            fixture.service.configure(serde_json::from_value(json!({"agentAccessEnabled":true})).unwrap()).await.unwrap();
            let opened=tools.open(&scope,input()).await.unwrap();assert_eq!(opened.device.id.as_str(),"fixture-ios");
            assert_eq!(&opened.agent_device.target_args[..4], &["--platform","ios","--udid","fixture-ios"]);
            assert_eq!(opened.agent_device.target_args[4],"--config");assert_eq!(opened.agent_device.target_args[6],"--session");
            let config:Value=serde_json::from_slice(&tokio::fs::read(&opened.agent_device.target_args[5]).await.unwrap()).unwrap();assert_eq!(config["daemonBaseUrl"],"http://127.0.0.1:12345");
            assert!(std::path::Path::new(&opened.agent_device.command).exists());
            let listed=tools.list(&scope,Some("remote")).await.unwrap();assert!(listed.devices.is_empty());assert_eq!(listed.open.len(),1);
            let shot=tools.screenshot(&scope,serde_json::from_value(json!({})).unwrap()).await.unwrap();assert_eq!(shot.device.id,opened.device.id);assert!(!shot.screenshot.data.is_empty());
            let thread=scope.thread.as_ref().unwrap().thread_id.clone();
            let mut other=scope.clone();other.thread.as_mut().unwrap().thread_id=ThreadId::new("other-thread").unwrap();
            assert_eq!(tools.screenshot(&other,serde_json::from_value(json!({})).unwrap()).await.unwrap_err().0["reason"],"No device is open in this thread. Call device_open first.");
            tools.close(&scope,serde_json::from_value(json!({})).unwrap()).await.unwrap();assert!(fixture.service.sessions_for_thread(&thread).is_empty());
            fixture.service.shutdown().await;fixture.settings.shutdown().await;
        }).await.expect("device MCP fixture timed out");
    }
}
