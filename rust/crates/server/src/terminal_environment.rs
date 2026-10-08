//! Shell fallback and terminal environment policy from the original manager.
//! Inherited application variables are removed before explicit overrides.
use t3_contracts::trim_wire_string;

pub type Environment = indexmap::IndexMap<String, String>;
/// Source mergePathEntries: preserve user/provider precedence and compare
/// trimmed entries literally, including on Windows.
pub fn append_managed_path(
    env: &mut Environment,
    directories: &[std::path::PathBuf],
    platform: &str,
) {
    if directories.is_empty() {
        return;
    }
    let delimiter = if platform == "win32" { ';' } else { ':' };
    let key = if platform == "win32" {
        env.keys()
            .find(|key| key.to_lowercase() == "path")
            .cloned()
            .unwrap_or_else(|| "PATH".into())
    } else {
        "PATH".into()
    };
    let added = directories
        .iter()
        .map(|path| path.to_string_lossy())
        .collect::<Vec<_>>()
        .join(&delimiter.to_string());
    let mut seen = std::collections::HashSet::new();
    let mut merged = Vec::new();
    for value in [env.get(&key).map(String::as_str), Some(added.as_str())]
        .into_iter()
        .flatten()
    {
        for entry in value.split(delimiter).map(trim_wire_string) {
            if !entry.is_empty() && seen.insert(entry.to_owned()) {
                merged.push(entry);
            }
        }
    }
    if !merged.is_empty() {
        env.insert(key, merged.join(&delimiter.to_string()));
    }
}
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ShellCandidate {
    pub shell: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
}
fn whitespace(c: char) -> bool {
    trim_wire_string(&c.to_string()).is_empty()
}
fn normalize(command: &str, platform: &str) -> Option<String> {
    let command = trim_wire_string(command);
    if command.is_empty() {
        return None;
    }
    if platform == "win32" {
        return Some(command.into());
    }
    let first = command.split(whitespace).next().unwrap_or("");
    // Source removes one quote at each end, including unmatched quotes.
    let first = first.strip_prefix(['\'', '"']).unwrap_or(first);
    let first = first.strip_suffix(['\'', '"']).unwrap_or(first);
    Some(first.into())
}
fn candidate(command: &str, platform: &str) -> Option<ShellCandidate> {
    if command.is_empty() {
        return None;
    }
    let normalized = command.replace('\\', "/");
    let name = normalized
        .split('/')
        .filter(|part| !part.is_empty())
        .next_back()
        .unwrap_or(&normalized)
        .to_lowercase();
    let args = if platform == "win32" && matches!(name.as_str(), "powershell.exe" | "pwsh.exe") {
        vec!["-NoLogo".into()]
    } else if platform != "win32" && name == "zsh" {
        vec!["-o".into(), "nopromptsp".into()]
    } else {
        vec![]
    };
    Some(ShellCandidate {
        shell: command.into(),
        args,
    })
}
fn windows_join(parts: &[&str]) -> String {
    parts
        .iter()
        .enumerate()
        .map(|(index, part)| {
            if index == 0 {
                part.trim_end_matches(['/', '\\'])
            } else {
                part.trim_matches(['/', '\\'])
            }
        })
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\\")
}
pub fn shell_candidates(
    requested: Option<&str>,
    platform: &str,
    env: &Environment,
) -> Vec<ShellCandidate> {
    let default = if platform == "win32" {
        "pwsh.exe"
    } else {
        env.get("SHELL").map(String::as_str).unwrap_or("bash")
    };
    let requested = normalize(requested.unwrap_or(default), platform);
    let mut commands = vec![requested];
    if platform == "win32" {
        // An empty SystemRoot falls through to windir in the source.
        let root = env
            .get("SystemRoot")
            .map(|v| trim_wire_string(v))
            .filter(|v| !v.is_empty())
            .or_else(|| {
                env.get("windir")
                    .map(|v| trim_wire_string(v))
                    .filter(|v| !v.is_empty())
            })
            .unwrap_or("C:\\Windows");
        commands.extend([
            Some("pwsh.exe".into()),
            Some(windows_join(&[
                root,
                "System32",
                "WindowsPowerShell",
                "v1.0",
                "powershell.exe",
            ])),
            Some("powershell.exe".into()),
            env.get("ComSpec").cloned(),
            Some(windows_join(&[root, "System32", "cmd.exe"])),
            Some("cmd.exe".into()),
        ]);
    } else {
        commands.push(
            env.get("SHELL")
                .and_then(|value| normalize(value, platform)),
        );
        commands.extend(
            ["/bin/zsh", "/bin/bash", "/bin/sh", "zsh", "bash", "sh"]
                .map(|value| Some(value.into())),
        );
    }
    let mut result = Vec::new();
    for command in commands.into_iter().flatten() {
        if let Some(candidate) = candidate(&command, platform) {
            // Source uniqueness includes arguments as part of the identity.
            if !result.contains(&candidate) {
                result.push(candidate);
            }
        }
    }
    result
}
fn excluded(key: &str) -> bool {
    let key = key.to_uppercase();
    key.starts_with("T3CODE_")
        || key.starts_with("VITE_")
        || matches!(
            key.as_str(),
            "PORT" | "ELECTRON_RENDERER_PORT" | "ELECTRON_RUN_AS_NODE"
        )
}
fn expanded_home(value: &str, home: &str, platform: &str) -> String {
    if value == "~" {
        return home.into();
    }
    if value.starts_with("~/") || value.starts_with("~\\") {
        // Node path.join normalizes . and .. segments. Absolute-looking suffixes
        // are joined too (path.resolve would incorrectly discard the home).
        let separator = if platform == "win32" { '\\' } else { '/' };
        let suffix = if platform == "win32" {
            value[2..].replace('/', "\\")
        } else {
            value[2..].to_string()
        };
        let joined = format!(
            "{}{}{}",
            home.trim_end_matches(separator),
            separator,
            suffix
        );
        let drive = platform == "win32"
            && joined.as_bytes().get(1) == Some(&b':')
            && joined.as_bytes().get(2) == Some(&b'\\');
        let unc = platform == "win32" && joined.starts_with("\\\\");
        let absolute = joined.starts_with(separator) || drive;
        let root_parts = if unc {
            2
        } else if drive {
            1
        } else {
            0
        };
        let mut parts = Vec::new();
        for part in joined.split(separator) {
            match part {
                "" | "." => {}
                ".." => {
                    if parts.len() > root_parts && parts.last().is_some_and(|last| *last != "..") {
                        parts.pop();
                    } else if !absolute {
                        parts.push(part);
                    }
                }
                _ => parts.push(part),
            }
        }
        let trailing = if drive && parts.len() == 1 {
            "\\".to_string()
        } else if parts.len() > root_parts && suffix.ends_with(separator) {
            separator.to_string()
        } else {
            String::new()
        };
        return format!(
            "{}{}{}",
            if unc {
                "\\\\".into()
            } else if absolute && !drive {
                separator.to_string()
            } else {
                String::new()
            },
            parts.join(&separator.to_string()),
            trailing
        );
    }
    value.into()
}
pub fn spawn_environment(
    base: &Environment,
    runtime: Option<&Environment>,
    platform: &str,
    home: &str,
) -> Environment {
    let mut env: Environment = base
        .iter()
        .filter(|(key, _)| !excluded(key))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    if let Some(runtime) = runtime {
        for (key, value) in runtime {
            let existing = if platform == "win32" {
                env.keys()
                    .find(|candidate| candidate.to_lowercase() == key.to_lowercase())
                    .cloned()
            } else {
                None
            };
            env.insert(
                existing.unwrap_or_else(|| key.clone()),
                if matches!(key.as_str(), "CODEX_HOME" | "CLAUDE_CONFIG_DIR") {
                    expanded_home(value, home, platform)
                } else {
                    value.clone()
                },
            );
        }
    }
    if env.get("COLORTERM").is_none_or(|value| value.is_empty())
        && runtime.is_none_or(|env| !env.contains_key("COLORTERM"))
    {
        env.insert("COLORTERM".into(), "truecolor".into());
    }
    if env.contains_key("APPIMAGE") || env.contains_key("APPDIR") {
        let app_dir = env
            .get("APPDIR")
            .map(|value| value.trim_end_matches('/').to_string());
        for key in ["APPIMAGE", "APPDIR", "ARGV0", "OWD"] {
            env.shift_remove(key);
        }
        if let Some(app_dir) = app_dir.filter(|value| !value.is_empty()) {
            for key in [
                "PATH",
                "LD_LIBRARY_PATH",
                "XDG_DATA_DIRS",
                "GSETTINGS_SCHEMA_DIR",
            ] {
                if let Some(value) = env.get(key) {
                    let prefix = format!("{app_dir}/");
                    let kept = value
                        .split(':')
                        .filter(|segment| {
                            !segment.is_empty()
                                && *segment != app_dir
                                && !segment.starts_with(&prefix)
                        })
                        .collect::<Vec<_>>()
                        .join(":");
                    if kept.is_empty() {
                        env.shift_remove(key);
                    } else {
                        env.insert(key.into(), kept);
                    }
                }
            }
        }
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn original_managed_path_append_oracle() {
        for line in include_str!("../tests/fixtures/managed-terminal-path.jsonl").lines() {
            let fixture: serde_json::Value = serde_json::from_str(line).unwrap();
            let mut env: Environment = serde_json::from_value(fixture["env"].clone()).unwrap();
            let directories: Vec<std::path::PathBuf> =
                serde_json::from_value(fixture["directories"].clone()).unwrap();
            append_managed_path(
                &mut env,
                &directories,
                fixture["platform"].as_str().unwrap(),
            );
            assert_eq!(
                serde_json::to_value(env).unwrap(),
                fixture["result"],
                "{fixture}"
            );
        }
    }
    #[test]
    fn original_shell_and_environment_policy_oracle() {
        let fixtures: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/terminal-environment.json"))
                .unwrap();
        for fixture in fixtures["shells"].as_array().unwrap() {
            let env = serde_json::from_value(fixture["env"].clone()).unwrap();
            let actual = shell_candidates(
                fixture["requested"].as_str(),
                fixture["platform"].as_str().unwrap(),
                &env,
            );
            assert_eq!(
                serde_json::to_value(actual).unwrap(),
                fixture["result"],
                "{fixture}"
            );
        }
        for fixture in fixtures["environments"].as_array().unwrap() {
            let base = serde_json::from_value(fixture["base"].clone()).unwrap();
            let runtime: Option<Environment> =
                serde_json::from_value(fixture["runtime"].clone()).unwrap();
            let actual = spawn_environment(
                &base,
                runtime.as_ref(),
                fixture["platform"].as_str().unwrap(),
                fixture["home"].as_str().unwrap(),
            );
            assert_eq!(
                serde_json::to_value(actual).unwrap(),
                fixture["result"],
                "{fixture}"
            );
        }
    }
}
