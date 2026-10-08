//! DeviceActions.ts command plans: argv stays typed; no shell proxy channel.
use crate::device_commands::HostCommandOutput;
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use t3_contracts::*;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionCommand {
    pub operation: String,
    pub command: String,
    pub args: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdin: Option<String>,
    pub ignore_failure: bool,
}
#[derive(Debug, Clone, Default)]
pub struct Helpers {
    pub node_path: String,
    pub serve_sim_ax_settings: Option<String>,
    pub serve_sim_cli: Option<String>,
}
fn unavailable(
    platform: DevicePlatform,
    operation: &str,
    reason: DeviceActionFailureReason,
) -> DeviceError {
    DeviceError::DeviceActionUnavailableError(DeviceActionUnavailableError {
        tag: DeviceActionUnavailableErrorTag::DeviceActionUnavailableError,
        operation: operation.into(),
        platform,
        reason,
    })
}
pub fn supports_action(platform: DevicePlatform, input: &DeviceActionInput) -> bool {
    let value = serde_json::to_value(input).unwrap();
    let kind = value["type"].as_str().unwrap();
    let supported = match platform {
        DevicePlatform::Ios => matches!(
            kind,
            "setAppearance"
                | "setTextSize"
                | "setToggle"
                | "setLiquidGlass"
                | "setColorFilter"
                | "setLocation"
                | "clearLocation"
                | "setPermission"
                | "openUrl"
                | "launchApp"
                | "terminateApp"
                | "sendPush"
        ),
        DevicePlatform::Android => matches!(
            kind,
            "setAppearance"
                | "setTextSize"
                | "setToggle"
                | "setOrientation"
                | "setLocation"
                | "clearLocation"
                | "setPermission"
                | "openUrl"
                | "launchApp"
                | "terminateApp"
        ),
    };
    supported
        && (kind != "setToggle"
            || match platform {
                DevicePlatform::Ios => matches!(
                    value["setting"].as_str(),
                    Some(
                        "reduceMotion"
                            | "increaseContrast"
                            | "reduceTransparency"
                            | "showBorders"
                            | "voiceOver"
                    )
                ),
                DevicePlatform::Android => matches!(
                    value["setting"].as_str(),
                    Some("reduceMotion" | "networkEnabled")
                ),
            })
}
fn command(operation: &str, program: &str, args: Vec<String>) -> ActionCommand {
    ActionCommand {
        operation: operation.into(),
        command: program.into(),
        args,
        stdin: None,
        ignore_failure: false,
    }
}
fn values(args: &[&str]) -> Vec<String> {
    args.iter().map(|value| (*value).into()).collect()
}
fn text_size(value: &str, ios: bool) -> &'static str {
    match (value, ios) {
        ("small", true) => "small",
        ("default", true) => "large",
        ("large", true) => "extra-extra-large",
        ("extra-large", true) => "accessibility-large",
        ("small", false) => "0.85",
        ("default", false) => "1.0",
        ("large", false) => "1.15",
        ("extra-large", false) => "1.3",
        _ => unreachable!(),
    }
}
// JSON.stringify visits canonical array-index keys first and formats numbers
// using ECMAScript semantics, including nested negative zero and exponent bounds.
fn stringify(value: &Value) -> String {
    match value {
        Value::Number(number) => t3_acp::js_number(number),
        Value::Array(values) => format!(
            "[{}]",
            values.iter().map(stringify).collect::<Vec<_>>().join(",")
        ),
        Value::Object(values) => {
            let mut numeric = Vec::new();
            let mut named = Vec::new();
            for (key, value) in values {
                let index = key
                    .parse::<u32>()
                    .ok()
                    .filter(|index| *index != u32::MAX && index.to_string() == *key);
                if let Some(index) = index {
                    numeric.push((index, key, value));
                } else {
                    named.push((key, value));
                }
            }
            numeric.sort_by_key(|(index, _, _)| *index);
            let entries = numeric
                .into_iter()
                .map(|(_, key, value)| (key, value))
                .chain(named);
            format!(
                "{{{}}}",
                entries
                    .map(|(key, value)| format!(
                        "{}:{}",
                        serde_json::to_string(key).unwrap(),
                        stringify(value)
                    ))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
        _ => serde_json::to_string(value).unwrap(),
    }
}
pub fn action_plan(
    platform: DevicePlatform,
    input: &DeviceActionInput,
    helpers: &Helpers,
) -> Result<Vec<ActionCommand>, DeviceError> {
    let value = serde_json::to_value(input).unwrap();
    let kind = value["type"].as_str().unwrap();
    if !supports_action(platform, input) {
        return Err(unavailable(
            platform,
            kind,
            DeviceActionFailureReason::Unsupported,
        ));
    }
    let id = value["deviceId"].as_str().unwrap();
    let string = |key: &str| value[key].as_str().unwrap();
    let number = |key: &str| t3_acp::js_number(value[key].as_number().unwrap());
    let simctl = |args: &[&str], operation: &str| {
        let mut argv = values(&["simctl", args[0], id]);
        argv.extend(values(&args[1..]));
        command(operation, "xcrun", argv)
    };
    let adb = |args: &[&str], operation: &str| {
        let mut argv = values(&["-s", id]);
        argv.extend(values(args));
        command(operation, "adb", argv)
    };
    let shell = |args: &[&str], operation: &str| {
        let mut argv = vec!["shell"];
        argv.extend(args.iter().copied());
        adb(&argv, operation)
    };
    let ax = |args: &[&str]| -> Result<ActionCommand, DeviceError> {
        let helper = helpers.serve_sim_ax_settings.as_deref().ok_or_else(|| {
            unavailable(
                platform,
                "accessibility",
                DeviceActionFailureReason::HelperMissing,
            )
        })?;
        let mut argv = values(&["simctl", "spawn", id, helper]);
        argv.extend(values(args));
        Ok(command("accessibility", "xcrun", argv))
    };
    let steps = if platform == DevicePlatform::Ios {
        match kind {
            "setAppearance" => vec![simctl(&["ui", "appearance", string("value")], "appearance")],
            "setTextSize" => vec![simctl(
                &["ui", "content_size", text_size(string("value"), true)],
                "text size",
            )],
            "setToggle" => {
                let enabled = value["value"].as_bool().unwrap();
                if string("setting") == "increaseContrast" {
                    vec![simctl(
                        &[
                            "ui",
                            "increase_contrast",
                            if enabled { "enabled" } else { "disabled" },
                        ],
                        "increase contrast",
                    )]
                } else {
                    let name = match string("setting") {
                        "reduceMotion" => "reduce-motion",
                        "reduceTransparency" => "reduce-transparency",
                        "showBorders" => "show-borders",
                        "voiceOver" => "voiceover",
                        _ => unreachable!(),
                    };
                    vec![ax(&["set", name, if enabled { "on" } else { "off" }])?]
                }
            }
            "setLiquidGlass" => vec![ax(&["set", "liquid-glass", string("value")])?],
            "setColorFilter" => vec![ax(&["set", "color-filter", string("value")])?],
            "setLocation" => vec![simctl(
                &[
                    "location",
                    "set",
                    &format!("{},{}", number("latitude"), number("longitude")),
                ],
                "location",
            )],
            "clearLocation" => vec![simctl(&["location", "clear"], "location")],
            "setPermission" => {
                let permission = string("permission");
                if permission == "notifications" {
                    let cli = helpers.serve_sim_cli.as_deref().ok_or_else(|| {
                        unavailable(
                            platform,
                            "permission",
                            DeviceActionFailureReason::HelperMissing,
                        )
                    })?;
                    vec![command(
                        "permission",
                        &helpers.node_path,
                        values(&[
                            cli,
                            "permissions",
                            string("decision"),
                            permission,
                            string("appId"),
                            "-d",
                            id,
                        ]),
                    )]
                } else {
                    vec![simctl(
                        &["privacy", string("decision"), permission, string("appId")],
                        "permission",
                    )]
                }
            }
            "openUrl" => vec![simctl(&["openurl", string("url")], "open url")],
            "launchApp" => vec![simctl(&["launch", string("appId")], "launch")],
            "terminateApp" => vec![simctl(&["terminate", string("appId")], "terminate")],
            "sendPush" => {
                let payload = if let Some(text) = value["payload"].as_str() {
                    json!({"aps":{"alert":text}})
                } else {
                    value["payload"].clone()
                };
                if !payload.is_object() {
                    return Err(DeviceError::DeviceOperationError(DeviceOperationError {
                        tag: DeviceOperationErrorTag::DeviceOperationError,
                        operation: "push".into(),
                        reason: DeviceOperationFailureReason::InvalidPayload,
                        exit_code: None,
                        cause: json!("Expected an object push payload."),
                    }));
                }
                let mut cmd = simctl(&["push", string("appId"), "-"], "push");
                cmd.stdin = Some(stringify(&payload));
                vec![cmd]
            }
            _ => unreachable!(),
        }
    } else {
        match kind {
            "setAppearance" => vec![shell(
                &[
                    "cmd",
                    "uimode",
                    "night",
                    if string("value") == "dark" {
                        "yes"
                    } else {
                        "no"
                    },
                ],
                "appearance",
            )],
            "setTextSize" => vec![shell(
                &[
                    "settings",
                    "put",
                    "system",
                    "font_scale",
                    text_size(string("value"), false),
                ],
                "text size",
            )],
            "setToggle" => {
                let enabled = value["value"].as_bool().unwrap();
                if string("setting") == "networkEnabled" {
                    ["wifi", "data"]
                        .iter()
                        .map(|service| {
                            shell(
                                &["svc", service, if enabled { "enable" } else { "disable" }],
                                "network",
                            )
                        })
                        .collect()
                } else {
                    [
                        "animator_duration_scale",
                        "transition_animation_scale",
                        "window_animation_scale",
                    ]
                    .iter()
                    .map(|key| {
                        shell(
                            &[
                                "settings",
                                "put",
                                "global",
                                key,
                                if enabled { "0" } else { "1" },
                            ],
                            "reduce motion",
                        )
                    })
                    .collect()
                }
            }
            "setOrientation" => {
                let index = match string("value") {
                    "portrait" => 0,
                    "landscape_left" => 1,
                    "portrait_upside_down" => 2,
                    "landscape_right" => 3,
                    _ => unreachable!(),
                };
                if id.starts_with("emulator-") {
                    vec![
                        shell(
                            &["settings", "put", "system", "accelerometer_rotation", "1"],
                            "orientation",
                        ),
                        shell(&["cmd", "window", "user-rotation", "free"], "orientation"),
                        adb(
                            &[
                                "emu",
                                "sensor",
                                "set",
                                "acceleration",
                                ["0:9.81:0", "9.81:0:0", "0:-9.81:0", "-9.81:0:0"][index],
                            ],
                            "orientation",
                        ),
                    ]
                } else {
                    vec![shell(
                        &[
                            "cmd",
                            "window",
                            "user-rotation",
                            "lock",
                            ["0", "1", "2", "3"][index],
                        ],
                        "orientation",
                    )]
                }
            }
            "setLocation" => vec![adb(
                &[
                    "emu",
                    "geo",
                    "fix",
                    &number("longitude"),
                    &number("latitude"),
                ],
                "location",
            )],
            "clearLocation" => vec![],
            "setPermission" => {
                let permissions: &[&str] = match string("permission") {
                    "camera" => &["CAMERA"],
                    "microphone" => &["RECORD_AUDIO"],
                    "photos" => &["READ_MEDIA_IMAGES", "READ_EXTERNAL_STORAGE"],
                    "contacts" => &["READ_CONTACTS", "WRITE_CONTACTS"],
                    "calendar" => &["READ_CALENDAR", "WRITE_CALENDAR"],
                    "location" => &["ACCESS_FINE_LOCATION", "ACCESS_COARSE_LOCATION"],
                    "notifications" => &["POST_NOTIFICATIONS"],
                    "motion" => &["ACTIVITY_RECOGNITION"],
                    _ => {
                        return Err(unavailable(
                            platform,
                            "permission",
                            DeviceActionFailureReason::Unsupported,
                        ));
                    }
                };
                permissions
                    .iter()
                    .map(|permission| {
                        let mut cmd = shell(
                            &[
                                "pm",
                                if string("decision") == "grant" {
                                    "grant"
                                } else {
                                    "revoke"
                                },
                                string("appId"),
                                &format!("android.permission.{permission}"),
                            ],
                            "permission",
                        );
                        cmd.ignore_failure = true;
                        cmd
                    })
                    .collect()
            }
            "openUrl" => vec![shell(
                &[
                    "am",
                    "start",
                    "-a",
                    "android.intent.action.VIEW",
                    "-d",
                    string("url"),
                ],
                "open url",
            )],
            "launchApp" => vec![shell(
                &[
                    "monkey",
                    "-p",
                    string("appId"),
                    "-c",
                    "android.intent.category.LAUNCHER",
                    "1",
                ],
                "launch",
            )],
            "terminateApp" => vec![shell(&["am", "force-stop", string("appId")], "terminate")],
            _ => unreachable!(),
        }
    };
    Ok(steps)
}

/// Source executes sequentially; selected Android permission failures are ignored.
pub async fn run_action(
    platform: DevicePlatform,
    input: &DeviceActionInput,
    helpers: &Helpers,
    run: &(impl Fn(ActionCommand) -> BoxFuture<'static, HostCommandOutput> + Send + Sync),
) -> Result<(), DeviceError> {
    for step in action_plan(platform, input, helpers)? {
        let operation = step.operation.clone();
        let ignore_failure = step.ignore_failure;
        let output = run(step).await;
        if output.code != 0 && !ignore_failure {
            return Err(DeviceError::DeviceOperationError(DeviceOperationError {
                tag: DeviceOperationErrorTag::DeviceOperationError,
                operation,
                reason: DeviceOperationFailureReason::CommandFailed,
                exit_code: Some(Some(output.code.into())),
                cause: json!({"code":output.code,"stdout":output.stdout,"stderr":output.stderr}),
            }));
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    #[tokio::test]
    async fn original_platform_commands_helpers_payloads_and_failure_order_match() {
        let mut failures = Vec::new();
        let mut count = 0;
        for (index, line) in include_str!("../tests/fixtures/device-actions.jsonl")
            .lines()
            .enumerate()
        {
            let fixture: Value = serde_json::from_str(line).unwrap();
            let input: DeviceActionInput =
                serde_json::from_value(fixture["input"].clone()).unwrap();
            let platform: DevicePlatform =
                serde_json::from_value(fixture["platform"].clone()).unwrap();
            let helpers = Helpers {
                node_path: fixture["helpers"]["nodePath"].as_str().unwrap().into(),
                serve_sim_ax_settings: fixture["helpers"]["serveSimAxSettings"]
                    .as_str()
                    .map(Into::into),
                serve_sim_cli: fixture["helpers"]["serveSimCli"].as_str().map(Into::into),
            };
            let calls = Arc::new(Mutex::new(Vec::new()));
            let retained = calls.clone();
            let code = fixture["code"].as_i64().unwrap() as i32;
            let result = run_action(platform, &input, &helpers, &move |step| {
                let mut value = json!({"command":step.command,"args":step.args});
                if let Some(stdin) = step.stdin {
                    value["stdin"] = json!(stdin);
                }
                retained.lock().unwrap().push(value);
                Box::pin(async move {
                    HostCommandOutput {
                        code,
                        stdout: "fixture stdout".into(),
                        stderr: "fixture stderr".into(),
                    }
                })
            })
            .await;
            let actual_calls = json!(*calls.lock().unwrap());
            let error = result
                .as_ref()
                .err()
                .map(|error| serde_json::to_value(error).unwrap());
            if supports_action(platform, &input) != fixture["supported"]
                || actual_calls != fixture["calls"]
                || result.is_ok() != fixture["ok"]
                || error.as_ref() != fixture.get("error")
            {
                failures.push(format!(
                    "case {index}: input={} calls={actual_calls} error={error:?}",
                    fixture["input"]
                ));
            }
            count += 1;
        }
        assert_eq!(count, 656);
        assert!(
            failures.is_empty(),
            "{} mismatches\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
}
