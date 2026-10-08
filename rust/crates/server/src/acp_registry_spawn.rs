//! Shared Windows spawn rules from packages/shared/src/shell.ts and Node's
//! child_process shell serializer. POSIX commands retain direct argv execution.
use indexmap::IndexMap;
#[cfg(any(windows, test))]
use serde::Serialize;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use tokio::process::Command;

type Environment = IndexMap<String, String>;
#[cfg(any(windows, test))]
fn extension(command: &str) -> &str {
    let name = command
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(command);
    let Some(dot) = name.rfind('.') else {
        return "";
    };
    if dot == 0 || name == ".." {
        ""
    } else {
        &name[dot..]
    }
}
#[cfg(any(windows, test))]
pub(crate) fn is_batch(command: &str) -> bool {
    matches!(
        extension(command).to_ascii_lowercase().as_str(),
        ".cmd" | ".bat"
    )
}
#[cfg(any(windows, test))]
fn escape_argument(argument: &str) -> String {
    let mut quoted = String::from("\"");
    let mut slashes = 0;
    for character in argument.chars() {
        if character == '\\' {
            slashes += 1;
            continue;
        }
        quoted.extend(std::iter::repeat_n(
            '\\',
            if character == '"' {
                2 * slashes + 1
            } else {
                slashes
            },
        ));
        slashes = 0;
        quoted.push(character);
    }
    quoted.extend(std::iter::repeat_n('\\', 2 * slashes));
    quoted.push('"');
    let mut escaped = String::new();
    for character in quoted.chars() {
        if "()[]%!^\"`<>&|;, *?".contains(character) {
            escaped.push('^');
        }
        escaped.push(character);
    }
    escaped
}
#[cfg(any(windows, test))]
fn extensions(environment: &Environment) -> Vec<String> {
    let mut result = Vec::new();
    if let Some(value) = environment.get("PATHEXT") {
        for entry in value
            .split(';')
            .map(t3_contracts::trim_wire_string)
            .filter(|entry| !entry.is_empty())
        {
            let extension = format!(
                "{}{}",
                if entry.starts_with('.') { "" } else { "." },
                entry.to_uppercase()
            );
            if !result.contains(&extension) {
                result.push(extension);
            }
        }
    }
    if result.is_empty() {
        result = [".COM", ".EXE", ".BAT", ".CMD"].map(String::from).into();
    }
    result
}
#[cfg(any(windows, test))]
fn candidates(command: &str, environment: &Environment) -> Vec<String> {
    let extensions = extensions(environment);
    let original = extension(command);
    let upper = original.to_uppercase();
    let mut result = Vec::new();
    let values = if !original.is_empty() && extensions.contains(&upper) {
        let stem = &command[..command.len() - original.len()];
        vec![
            command.to_owned(),
            format!("{stem}{upper}"),
            format!("{stem}{}", upper.to_lowercase()),
        ]
    } else {
        extensions
            .into_iter()
            .flat_map(|extension| {
                [
                    format!("{command}{extension}"),
                    format!("{command}{}", extension.to_lowercase()),
                ]
            })
            .collect()
    };
    for value in values {
        if !result.contains(&value) {
            result.push(value);
        }
    }
    result
}
pub(crate) fn preferred_path(
    environment: &Environment,
    directory: &str,
    windows: bool,
) -> Environment {
    let inherited = environment.get("PATH").or_else(|| {
        windows
            .then(|| {
                environment
                    .iter()
                    .find(|(key, _)| key.to_lowercase() == "path")
                    .map(|(_, value)| value)
            })
            .flatten()
    });
    let delimiter = if windows { ';' } else { ':' };
    let mut entries = Vec::new();
    for value in [Some(directory), inherited.map(String::as_str)]
        .into_iter()
        .flatten()
    {
        for entry in value.split(delimiter).map(t3_contracts::trim_wire_string) {
            if !entry.is_empty() && !entries.contains(&entry) {
                entries.push(entry);
            }
        }
    }
    let mut result = environment.clone();
    if !entries.is_empty() {
        result.insert("PATH".into(), entries.join(&delimiter.to_string()));
    } else {
        result.shift_remove("PATH");
    }
    result
}
#[cfg(any(windows, test))]
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Plan {
    command: String,
    args: Vec<String>,
    verbatim: bool,
    env_pairs: Vec<String>,
}
#[cfg(any(windows, test))]
fn plan(
    command: &str,
    args: &[String],
    resolved: Option<&str>,
    environment: &Environment,
    comspec: Option<&str>,
) -> Plan {
    let command = resolved.unwrap_or(command);
    let keys = environment_keys(environment);
    let env_pairs = keys
        .into_iter()
        .map(|key| format!("{key}={}", environment[key]))
        .collect();
    if !is_batch(command) {
        return Plan {
            command: command.into(),
            args: args.to_vec(),
            verbatim: false,
            env_pairs,
        };
    }
    let shell = comspec
        .filter(|value| !value.is_empty())
        .unwrap_or("cmd.exe");
    let mut line = escape_argument(command);
    for argument in args {
        line.push(' ');
        line.push_str(&escape_argument(argument));
    }
    // Node's shell check recognizes backslash-qualified cmd.exe, not slash paths.
    let basename = shell.rsplit('\\').next().unwrap_or(shell);
    let verbatim = basename.eq_ignore_ascii_case("cmd") || basename.eq_ignore_ascii_case("cmd.exe");
    Plan {
        command: shell.into(),
        args: if verbatim {
            vec!["/d".into(), "/s".into(), "/c".into(), format!("\"{line}\"")]
        } else {
            vec!["-c".into(), line]
        },
        verbatim,
        env_pairs,
    }
}
#[cfg(any(windows, test))]
fn environment_keys(environment: &Environment) -> Vec<&String> {
    let mut keys = environment.keys().collect::<Vec<_>>();
    keys.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
    let mut seen = std::collections::HashSet::new();
    keys.into_iter()
        .filter(|key| seen.insert(key.to_uppercase()))
        .collect()
}
/// Source PATH lookup accepts PATH, Path and path, strips wrapping quotes, and
/// checks only PATHEXT-admitted regular files on Windows.
pub(crate) fn resolve_executable(command: &str, environment: &Environment) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let candidates = candidates(command, environment);
        let valid = |path: &Path| {
            path.is_file()
                && extensions(environment)
                    .contains(&extension(&path.to_string_lossy()).to_uppercase())
        };
        if command.contains(['/', '\\']) {
            return candidates
                .into_iter()
                .map(PathBuf::from)
                .find(|path| valid(path));
        }
        for directory in environment
            .get("PATH")
            .or_else(|| environment.get("Path"))
            .or_else(|| environment.get("path"))
            .into_iter()
            .flat_map(|value| value.split(';'))
        {
            let directory = t3_contracts::trim_wire_string(directory).trim_matches('"');
            if directory.is_empty() {
                continue;
            }
            for candidate in &candidates {
                let path = Path::new(directory).join(candidate);
                if valid(&path) {
                    return Some(path);
                }
            }
        }
        None
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        let valid = |path: &Path| {
            path.metadata().is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        };
        if command.contains(['/', '\\']) {
            return valid(Path::new(command)).then(|| PathBuf::from(command));
        }
        environment
            .get("PATH")
            .into_iter()
            .flat_map(|value| value.split(':'))
            .map(t3_contracts::trim_wire_string)
            .map(|value| value.trim_matches('"'))
            .filter(|value| !value.is_empty())
            .map(|directory| Path::new(directory).join(command))
            .find(|path| valid(path))
    }
}
/// `environment=None` inherits the host. Supplied environments are complete;
/// callers which extend the host merge it before this boundary.
pub(crate) fn command(
    command: &str,
    arguments: &[String],
    environment: Option<&Environment>,
) -> Command {
    #[cfg(windows)]
    {
        let inherited;
        let environment = match environment {
            Some(value) => value,
            None => {
                inherited = std::env::vars().collect();
                &inherited
            }
        };
        let resolved = resolve_executable(command, environment);
        let resolved = resolved.as_ref().map(|path| path.to_string_lossy());
        let plan = plan(
            command,
            arguments,
            resolved.as_deref(),
            environment,
            std::env::var("comspec").ok().as_deref(),
        );
        let mut builder = Command::new(&plan.command);
        if plan.verbatim {
            builder.args(&plan.args[..3]);
            builder.as_std_mut().raw_arg(&plan.args[3]);
        } else {
            builder.args(&plan.args);
        }
        builder.env_clear();
        for key in environment_keys(environment) {
            builder.env(key, &environment[key]);
        }
        builder
    }
    #[cfg(not(windows))]
    {
        let mut builder = Command::new(command);
        builder.args(arguments);
        if let Some(environment) = environment {
            builder.env_clear().envs(environment);
        }
        builder
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    #[test]
    fn original_windows_spawn_and_path_fixtures() {
        for line in include_str!("../tests/fixtures/acp-registry-windows-spawn.jsonl").lines() {
            let row: Value = serde_json::from_str(line).unwrap();
            let environment: Environment =
                serde_json::from_value(row["environment"].clone()).unwrap();
            let actual = match row["operation"].as_str().unwrap() {
                "plan" => serde_json::to_value(plan(
                    row["command"].as_str().unwrap(),
                    &serde_json::from_value::<Vec<String>>(row["args"].clone()).unwrap(),
                    row["resolved"].as_str(),
                    &environment,
                    row["comspec"].as_str(),
                ))
                .unwrap(),
                "candidates" => {
                    serde_json::to_value(candidates(row["command"].as_str().unwrap(), &environment))
                        .unwrap()
                }
                "path" => serde_json::to_value(preferred_path(
                    &environment,
                    row["directory"].as_str().unwrap(),
                    row["windows"].as_bool().unwrap(),
                ))
                .unwrap(),
                _ => panic!("unknown operation"),
            };
            assert_eq!(actual, row["output"], "{row}");
        }
    }
}
