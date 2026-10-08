//! Host diagnostics and helper environment from LocalDeviceHost.ts.
use crate::terminal_environment::Environment;
use std::path::{Path, PathBuf};
use t3_contracts::{DevicePlatform, DevicePlatformAvailability, trim_wire_string};
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AndroidSdk {
    pub root: Option<PathBuf>,
    pub adb: bool,
    pub emulator: bool,
    pub avdmanager: bool,
    pub legacy_avdmanager: bool,
}
impl AndroidSdk {
    fn absent() -> Self {
        Self {
            root: None,
            adb: false,
            emulator: false,
            avdmanager: false,
            legacy_avdmanager: false,
        }
    }
}
fn join(platform: &str, root: &Path, parts: &[&str]) -> PathBuf {
    let windows = platform == "win32";
    let separator = if windows { '\\' } else { '/' };
    let root = root.to_string_lossy();
    let joined = std::iter::once(root.as_ref())
        .chain(parts.iter().copied())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(&separator.to_string());
    let mut text = if windows {
        joined.replace('/', "\\")
    } else {
        joined
    };
    if windows {
        let first = root.replace('/', "\\");
        let unc =
            first.starts_with("\\\\") && first.as_bytes().get(2).is_some_and(|byte| *byte != b'\\');
        if !unc && text.starts_with("\\\\") {
            text = format!("\\{}", text.trim_start_matches('\\'));
        }
    }
    let mut prefix = String::new();
    let mut remainder = text.as_str();
    let mut absolute = remainder.starts_with(separator);
    if windows {
        let bytes = text.as_bytes();
        if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            prefix = text[..2].into();
            remainder = &text[2..];
            absolute = remainder.starts_with(separator);
        } else if text.starts_with("\\\\") {
            let names = text[2..]
                .split('\\')
                .filter(|part| !part.is_empty())
                .take(2)
                .collect::<Vec<_>>();
            if names.len() == 2 {
                prefix = format!("\\\\{}\\{}", names[0], names[1]);
                let offset = text[2..].find(names[1]).unwrap() + 2 + names[1].len();
                remainder = &text[offset..];
                absolute = true;
            }
        }
    }
    let trailing = remainder.ends_with(separator);
    let mut normalized: Vec<&str> = Vec::new();
    for part in remainder.split(separator) {
        match part {
            "" | "." => {}
            ".." if normalized.last().is_some_and(|last| *last != "..") => {
                normalized.pop();
            }
            ".." if absolute => {}
            other => normalized.push(other),
        }
    }
    let mut result = prefix;
    if absolute {
        result.push(separator);
    }
    result.push_str(&normalized.join(&separator.to_string()));
    if result.is_empty() {
        result.push('.');
    }
    if trailing && !result.ends_with(separator) {
        result.push(separator);
    }
    result.into()
}
async fn exists(path: &Path) -> bool {
    tokio::fs::try_exists(path).await.unwrap_or(false)
}
pub async fn android_sdk(environment: &Environment, platform: &str) -> AndroidSdk {
    let home = environment
        .get("HOME")
        .or_else(|| environment.get("USERPROFILE"))
        .map(String::as_str)
        .unwrap_or("");
    let explicit = environment
        .get("ANDROID_HOME")
        .map(String::as_str)
        .map(trim_wire_string)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            environment
                .get("ANDROID_SDK_ROOT")
                .map(String::as_str)
                .map(trim_wire_string)
                .filter(|value| !value.is_empty())
        });
    let exe = if platform == "win32" {
        "adb.exe"
    } else {
        "adb"
    };
    let mut candidates = if let Some(root) = explicit {
        vec![PathBuf::from(root)]
    } else {
        let local = environment
            .get("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| join(platform, Path::new(home), &["AppData", "Local"]));
        vec![
            join(platform, Path::new(home), &["Library", "Android", "sdk"]),
            join(platform, Path::new(home), &["Android", "Sdk"]),
            join(platform, &local, &["Android", "Sdk"]),
        ]
    };
    if explicit.is_none() {
        for directory in environment
            .get("PATH")
            .map(String::as_str)
            .unwrap_or("")
            .split(if platform == "win32" { ';' } else { ':' })
        {
            if directory.is_empty() {
                continue;
            }
            if let Ok(path) =
                tokio::fs::canonicalize(join(platform, Path::new(directory), &[exe])).await
            {
                if let Some(root) = path.parent().and_then(Path::parent) {
                    candidates.push(root.to_path_buf());
                }
            }
        }
    }
    for root in candidates {
        let adb = exists(&join(platform, &root, &["platform-tools", exe])).await;
        let emulator = exists(&join(
            platform,
            &root,
            &[
                "emulator",
                if platform == "win32" {
                    "emulator.exe"
                } else {
                    "emulator"
                },
            ],
        ))
        .await;
        if explicit.is_some() || adb || emulator {
            let manager = if platform == "win32" {
                "avdmanager.bat"
            } else {
                "avdmanager"
            };
            let avdmanager = exists(&join(
                platform,
                &root,
                &["cmdline-tools", "latest", "bin", manager],
            ))
            .await;
            let legacy_avdmanager =
                !avdmanager && exists(&join(platform, &root, &["tools", "bin", manager])).await;
            return AndroidSdk {
                root: Some(root),
                adb,
                emulator,
                avdmanager,
                legacy_avdmanager,
            };
        }
    }
    AndroidSdk::absent()
}
pub async fn platform_availability(
    platform: DevicePlatform,
    environment: &Environment,
    host_platform: &str,
) -> DevicePlatformAvailability {
    let reason = match platform {
        DevicePlatform::Ios if host_platform != "darwin" => {
            Some("iOS Simulators need macOS with Xcode.".into())
        }
        DevicePlatform::Ios => {
            if crate::acp_registry_spawn::resolve_executable("xcrun", environment).is_some() {
                None
            } else {
                Some("Xcode command line tools were not found.".into())
            }
        }
        DevicePlatform::Android => {
            let sdk = android_sdk(environment, host_platform).await;
            match sdk.root{
   None=>Some("Android SDK was not found. Install it with Android Studio or set ANDROID_HOME to your SDK directory.".into()),
   Some(root)=>{let root=root.to_string_lossy();if !sdk.adb{Some(format!("Android SDK Platform-Tools are missing from {root}. Install them in Android Studio's SDK Manager."))}else if !sdk.emulator{Some(format!("Android Emulator is missing from {root}. Install it in Android Studio's SDK Manager."))}else if !sdk.avdmanager{Some(if sdk.legacy_avdmanager{format!("The Android SDK command-line tools in {root} appear to be an older, unsupported version. Install Android SDK Command-line Tools (latest) in Android Studio's SDK Manager under SDK Tools.")}else{format!("Android SDK Command-line Tools (latest) are missing from {root}. Install them in Android Studio's SDK Manager.")})}else{None}}
  }
        }
    };
    DevicePlatformAvailability {
        platform,
        available: reason.is_none(),
        reason: reason.map(Some),
    }
}
pub fn host_environment(
    mut environment: Environment,
    sdk_root: Option<&Path>,
    platform: &str,
) -> Environment {
    if let Some(root) = sdk_root.filter(|root| !root.as_os_str().is_empty()) {
        environment.insert("ANDROID_HOME".into(), root.to_string_lossy().into_owned());
        let inherited = environment
            .get("PATH")
            .or_else(|| environment.get("Path"))
            .cloned()
            .unwrap_or_default();
        environment.insert(
            "PATH".into(),
            [
                join(platform, root, &["platform-tools"])
                    .to_string_lossy()
                    .into_owned(),
                join(platform, root, &["emulator"])
                    .to_string_lossy()
                    .into_owned(),
                inherited,
            ]
            .join(if platform == "win32" { ";" } else { ":" }),
        );
    }
    environment
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_node_path_and_helper_environment_oracle() {
        let mut failures = Vec::new();
        for line in include_str!("../tests/fixtures/device-platform.jsonl").lines() {
            let fixture: serde_json::Value = serde_json::from_str(line).unwrap();
            let platform = fixture["platform"].as_str().unwrap();
            let actual = if fixture["type"] == "join" {
                let parts = fixture["parts"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|part| part.as_str().unwrap())
                    .collect::<Vec<_>>();
                serde_json::json!(
                    join(
                        platform,
                        Path::new(fixture["root"].as_str().unwrap()),
                        &parts
                    )
                    .to_string_lossy()
                )
            } else {
                let env = serde_json::from_value(fixture["env"].clone()).unwrap();
                serde_json::to_value(host_environment(
                    env,
                    fixture["root"].as_str().map(Path::new),
                    platform,
                ))
                .unwrap()
            };
            if actual != fixture["result"] {
                failures.push(format!("{fixture}: actual={actual}"));
            }
        }
        assert!(
            failures.is_empty(),
            "{} mismatches\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
    #[tokio::test]
    async fn explicit_sdk_has_precedence_even_incomplete_and_diagnostics_follow_source_order() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sdk");
        let env = Environment::from_iter([
            ("HOME".into(), temp.path().to_string_lossy().into_owned()),
            ("PATH".into(), String::new()),
            ("ANDROID_HOME".into(), format!(" {} ", root.display())),
            (
                "ANDROID_SDK_ROOT".into(),
                temp.path().join("other").to_string_lossy().into_owned(),
            ),
        ]);
        let sdk = android_sdk(&env, "darwin").await;
        assert_eq!(sdk.root.as_deref(), Some(root.as_path()));
        assert!(!sdk.adb);
        let expected = [
            ("platform-tools/adb", "Platform-Tools"),
            ("emulator/emulator", "Android Emulator"),
            ("tools/bin/avdmanager", "older, unsupported"),
            ("cmdline-tools/latest/bin/avdmanager", ""),
        ];
        for (index, (file, reason)) in expected.iter().enumerate() {
            let path = root.join(file);
            tokio::fs::create_dir_all(path.parent().unwrap())
                .await
                .unwrap();
            tokio::fs::write(path, "").await.unwrap();
            let availability = platform_availability(DevicePlatform::Android, &env, "darwin").await;
            if index == 3 {
                assert!(availability.available);
            } else {
                let expected = if index == 0 {
                    "Android Emulator"
                } else if index == 1 {
                    "Command-line Tools (latest) are missing"
                } else {
                    reason
                };
                assert!(availability.reason.unwrap().unwrap().contains(expected));
            }
        }
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn path_adb_symlink_resolves_sdk_without_any_tool_execution() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sdk");
        let adb = root.join("platform-tools/adb");
        tokio::fs::create_dir_all(adb.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&adb, "").await.unwrap();
        let bin = temp.path().join("bin");
        tokio::fs::create_dir(&bin).await.unwrap();
        symlink(&adb, bin.join("adb")).unwrap();
        let env = Environment::from_iter([
            (
                "HOME".into(),
                temp.path().join("home").to_string_lossy().into_owned(),
            ),
            ("PATH".into(), bin.to_string_lossy().into_owned()),
        ]);
        assert_eq!(
            android_sdk(&env, "darwin").await.root,
            Some(tokio::fs::canonicalize(&root).await.unwrap())
        );
        let merged = host_environment(env, Some(&root), "darwin");
        assert_eq!(
            merged["PATH"],
            format!(
                "{}/platform-tools:{}/emulator:{}",
                root.display(),
                root.display(),
                bin.display()
            )
        );
        assert!(
            !platform_availability(DevicePlatform::Ios, &Environment::new(), "linux")
                .await
                .available
        );
    }
}
