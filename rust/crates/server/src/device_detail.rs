//! Parallel typed settings reads; failed commands degrade to unknown fields.
use crate::{
    device_actions::{ActionCommand, Helpers},
    device_commands::HostCommandOutput,
};
use futures_util::future::{BoxFuture, join_all};
use serde_json::{Value, json};
use t3_contracts::{DeviceForegroundApp, DevicePlatform, DeviceSettings, trim_wire_string};

fn command(program: &str, args: Vec<String>) -> ActionCommand {
    ActionCommand {
        command: program.into(),
        args,
        operation: "read settings".into(),
        stdin: None,
        ignore_failure: false,
    }
}
pub fn read_commands(platform: DevicePlatform, id: &str, helpers: &Helpers) -> Vec<ActionCommand> {
    if platform == DevicePlatform::Ios {
        let mut result = ["appearance", "content_size", "increase_contrast"]
            .into_iter()
            .map(|option| {
                command(
                    "xcrun",
                    vec!["simctl".into(), "ui".into(), id.into(), option.into()],
                )
            })
            .collect::<Vec<_>>();
        if let Some(helper) = &helpers.serve_sim_ax_settings {
            result.push(command(
                "xcrun",
                vec![
                    "simctl".into(),
                    "spawn".into(),
                    id.into(),
                    helper.clone(),
                    "status".into(),
                ],
            ));
        }
        result
    } else {
        [
            vec!["cmd", "uimode", "night"],
            vec!["settings", "get", "system", "font_scale"],
            vec!["settings", "get", "global", "animator_duration_scale"],
            vec!["settings", "get", "global", "wifi_on"],
            vec!["dumpsys", "window"],
        ]
        .into_iter()
        .map(|args| {
            command(
                "adb",
                [
                    vec!["-s".into(), id.into(), "shell".into()],
                    args.into_iter().map(String::from).collect(),
                ]
                .concat(),
            )
        })
        .collect()
    }
}
fn js_number(value: &str) -> f64 {
    let value = trim_wire_string(value);
    if value.is_empty() {
        return 0.0;
    }
    if matches!(value, "Infinity" | "+Infinity") {
        return f64::INFINITY;
    }
    if value == "-Infinity" {
        return f64::NEG_INFINITY;
    }
    for (prefix, radix) in [
        ("0x", 16),
        ("0X", 16),
        ("0b", 2),
        ("0B", 2),
        ("0o", 8),
        ("0O", 8),
    ] {
        if let Some(digits) = value.strip_prefix(prefix) {
            if digits.is_empty() {
                return f64::NAN;
            }
            return digits
                .chars()
                .try_fold(0.0, |number, digit| {
                    digit
                        .to_digit(radix)
                        .map(|digit| number * f64::from(radix) + f64::from(digit))
                })
                .unwrap_or(f64::NAN);
        }
    }
    if !regex::Regex::new(r"^[+-]?(?:[0-9]+(?:\.[0-9]*)?|\.[0-9]+)(?:[eE][+-]?[0-9]+)?$")
        .unwrap()
        .is_match(value)
    {
        return f64::NAN;
    }
    value.parse().unwrap_or(f64::NAN)
}
pub fn parse_detail(
    platform: DevicePlatform,
    outputs: &[Option<String>],
) -> (DeviceSettings, Option<DeviceForegroundApp>) {
    let value = |index: usize| {
        outputs
            .get(index)
            .and_then(Option::as_deref)
            .map(trim_wire_string)
    };
    let mut settings = json!({});
    let mut foreground = None;
    if platform == DevicePlatform::Ios {
        let appearance = value(0).map(str::to_lowercase);
        if let Some(appearance @ ("light" | "dark")) = appearance.as_deref() {
            settings["appearance"] = json!(appearance);
        }
        if let Some(size) = value(1)
            .filter(|size| !size.is_empty())
            .map(str::to_lowercase)
        {
            settings["textSize"] = json!(match size.as_str() {
                "small" => "small",
                "large" => "default",
                "extra-extra-large" => "large",
                "accessibility-large" => "extra-large",
                _ if size.starts_with("accessibility") => "extra-large",
                _ if size.contains("extra") => "large",
                "medium" => "small",
                _ => "default",
            });
        }
        if let Some(contrast) = value(2).filter(|value| !value.is_empty()) {
            settings["increaseContrast"] = json!(contrast.to_lowercase() == "enabled");
        }
        let status = outputs
            .get(3)
            .and_then(Option::as_deref)
            .and_then(|value| serde_json::from_str::<Value>(value).ok())
            .filter(|value| {
                value
                    .as_object()
                    .is_some_and(|object| object.values().all(Value::is_string))
            });
        if let Some(status) = status {
            for (native, field) in [
                ("reduce-motion", "reduceMotion"),
                ("reduce-transparency", "reduceTransparency"),
                ("show-borders", "showBorders"),
                ("voiceover", "voiceOver"),
            ] {
                if let Some(value @ ("on" | "off")) = status[native].as_str() {
                    settings[field] = json!(value == "on");
                }
            }
            if let Some(value @ ("clear" | "tinted")) = status["liquid-glass"].as_str() {
                settings["liquidGlass"] = json!(value);
            }
            if let Some(
                value @ ("none" | "grayscale" | "red-green" | "green-red" | "blue-yellow"),
            ) = status["color-filter"].as_str()
            {
                settings["colorFilter"] = json!(value);
            }
        }
    } else {
        if let Some(night) = value(0) {
            if night.contains("yes") {
                settings["appearance"] = json!("dark");
            } else if night.contains("no") {
                settings["appearance"] = json!("light");
            }
        }
        if let Some(scale) = value(1)
            .filter(|value| !value.is_empty() && *value != "null")
            .map(js_number)
            .filter(|value| value.is_finite())
        {
            settings["textSize"] = json!(if scale <= 0.9 {
                "small"
            } else if scale >= 1.25 {
                "extra-large"
            } else if scale >= 1.1 {
                "large"
            } else {
                "default"
            });
        }
        if let Some(animator) = value(2).filter(|value| *value != "null") {
            settings["reduceMotion"] = json!(js_number(animator) == 0.0);
        }
        if let Some(wifi @ ("1" | "0")) = value(3) {
            settings["networkEnabled"] = json!(wifi == "1");
        }
        if let Some(focus) = value(4).and_then(|focus| {
            regex::Regex::new(
                r"m(?:CurrentFocus|FocusedApp)=[A-Za-z0-9_]+\{[^ ]+ u[0-9]+ ([^/ ]+)/",
            )
            .unwrap()
            .captures(focus)
        }) {
            foreground = Some(DeviceForegroundApp {
                id: focus[1].into(),
                name: None,
                version: None,
            });
        }
    }
    (
        serde_json::from_value(settings).expect("known device setting fields"),
        foreground,
    )
}
pub async fn read_detail(
    platform: DevicePlatform,
    id: &str,
    helpers: &Helpers,
    run: &(impl Fn(ActionCommand) -> BoxFuture<'static, HostCommandOutput> + Send + Sync),
) -> (DeviceSettings, Option<DeviceForegroundApp>) {
    let outputs = join_all(
        read_commands(platform, id, helpers)
            .into_iter()
            .map(|command| async {
                let output = run(command).await;
                (output.code == 0).then_some(output.stdout)
            }),
    )
    .await;
    parse_detail(platform, &outputs)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn original_parallel_device_settings_commands_and_unknown_fallbacks_match() {
        let mut failures = Vec::new();
        let mut count = 0;
        for (index, line) in include_str!("../tests/fixtures/device-detail.jsonl")
            .lines()
            .enumerate()
        {
            let fixture: Value = serde_json::from_str(line).unwrap();
            let platform: DevicePlatform =
                serde_json::from_value(fixture["platform"].clone()).unwrap();
            let helpers = Helpers {
                serve_sim_ax_settings: fixture["helper"].as_str().map(Into::into),
                ..Default::default()
            };
            let commands = read_commands(platform, "fixture-id", &helpers);
            let calls = json!(
                commands
                    .iter()
                    .map(|command| json!({"command":command.command,"args":command.args}))
                    .collect::<Vec<_>>()
            );
            let mut outputs =
                serde_json::from_value::<Vec<Option<String>>>(fixture["outputs"].clone()).unwrap();
            outputs.truncate(commands.len());
            let (settings, foreground) = parse_detail(platform, &outputs);
            let actual = json!({"settings":settings,"foregroundApp":foreground});
            if actual != fixture["result"] || calls != fixture["calls"] {
                failures.push(format!(
                    "{index}: {actual} expected {} calls={calls}",
                    fixture["result"]
                ));
            }
            count += 1;
        }
        assert_eq!(count, 142);
        assert!(
            failures.is_empty(),
            "{} mismatches\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
    #[tokio::test]
    async fn independent_reads_are_admitted_before_any_response_completes() {
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(5));
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let retained = calls.clone();
        let run = move |_| {
            let barrier = barrier.clone();
            let calls = calls.clone();
            Box::pin(async move {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                barrier.wait().await;
                HostCommandOutput {
                    code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                }
            }) as BoxFuture<'static, HostCommandOutput>
        };
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            read_detail(
                DevicePlatform::Android,
                "fixture-id",
                &Helpers::default(),
                &run,
            ),
        )
        .await
        .unwrap();
        assert_eq!(retained.load(std::sync::atomic::Ordering::SeqCst), 5);
    }
}
