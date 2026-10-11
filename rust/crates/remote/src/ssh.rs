//! SSH helpers (`packages/ssh/src/command.ts`, `config.ts`): turning an alias into a target,
//! building ssh arguments, finding hosts to offer, and running one ssh command. The remote
//! launcher (`tunnel.ts`: starting `t3` on the remote, forwarding ports, pairing) is
//! [`crate::ssh_tunnel`] with its script text in [`crate::ssh_scripts`], and the askpass password
//! helpers (`auth.ts`) are [`crate::ssh_auth`]; they use this module's target type, argument
//! builders and redaction.
//!
//! TODO(original packages/ssh/src/command.ts `buildSshHostSpec`): the destination is passed to ssh
//! as it is, without `--` before it, so an alias that starts with `-` (a `Host` entry or
//! known_hosts name such as `-oProxyCommand=...`, or one typed by the user) is read by ssh as an
//! option. Copied from the original rather than fixed here; a fix has to reject such aliases in
//! [`build_ssh_host_spec`] (callers already handle its error).

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const DEFAULT_SSH_COMMAND_TIMEOUT: Duration = Duration::from_millis(60_000);
const MAX_SSH_ERROR_OUTPUT_LENGTH: usize = 4_000;

/// `DesktopSshEnvironmentTarget`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshTarget {
    pub alias: String,
    pub hostname: String,
    pub username: Option<String>,
    pub port: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HostSource {
    SshConfig,
    KnownHosts,
}

/// `DesktopDiscoveredSshHost`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredSshHost {
    pub alias: String,
    pub hostname: String,
    pub username: Option<String>,
    pub port: Option<i64>,
    pub source: HostSource,
}

/// `ssh` is a real executable everywhere (`ssh.exe` on Windows); it is spawned directly because a
/// shell would re-tokenise arguments such as identity-file paths containing spaces.
pub fn ssh_command_for_platform(windows: bool) -> &'static str {
    if windows { "ssh.exe" } else { "ssh" }
}

/// `parseSshResolveOutput`: reads `ssh -G <alias>` output.
pub fn parse_ssh_resolve_output(alias: &str, stdout: &str) -> SshTarget {
    let mut values: BTreeMap<&str, String> = BTreeMap::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut words = line.split_whitespace();
        let Some(key) = words.next() else { continue };
        let rest: Vec<&str> = words.collect();
        if rest.is_empty() || values.contains_key(key) {
            continue;
        }
        values.insert(key, rest.join(" ").trim().to_owned());
    }
    let nonempty = |key: &str| values.get(key).map(|v| v.trim()).filter(|v| !v.is_empty());
    SshTarget {
        alias: alias.to_owned(),
        hostname: nonempty("hostname").unwrap_or(alias).to_owned(),
        username: nonempty("user").map(str::to_owned),
        port: values
            .get("port")
            .and_then(|raw| crate::util::js_parse_int(raw.trim())),
    }
}

pub fn target_connection_key(target: &SshTarget) -> String {
    format!(
        "{}\u{0}{}\u{0}{}\u{0}{}",
        target.alias,
        target.hostname,
        target.username.as_deref().unwrap_or(""),
        target.port.map(|p| p.to_string()).unwrap_or_default()
    )
}

/// Names the remote state directory for a target: the first 16 hex chars of the SHA-256 of its
/// connection key. Remote servers store state under it, so it must never change across releases.
pub fn remote_state_key(target: &SshTarget) -> String {
    let digest = Sha256::digest(target_connection_key(target).as_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct SshInvalidTarget {
    pub message: String,
}

pub fn build_ssh_host_spec(target: &SshTarget) -> Result<String, SshInvalidTarget> {
    let destination = if target.alias.trim().is_empty() {
        target.hostname.trim()
    } else {
        target.alias.trim()
    };
    if destination.is_empty() {
        return Err(SshInvalidTarget {
            message: "SSH target is missing its alias/hostname.".into(),
        });
    }
    Ok(match &target.username {
        Some(user) if !user.is_empty() => format!("{user}@{destination}"),
        _ => destination.to_owned(),
    })
}

pub fn base_ssh_args(target: &SshTarget, batch_mode: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "-o".to_owned(),
        format!("BatchMode={}", batch_mode.unwrap_or("no")),
        "-o".to_owned(),
        "ConnectTimeout=10".to_owned(),
    ];
    if let Some(port) = target.port {
        args.push("-p".into());
        args.push(port.to_string());
    }
    args
}

pub fn get_last_non_empty_output_line(stdout: &str) -> Option<String> {
    stdout
        .trim()
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .map(str::to_owned)
}

/// Hides credentials in JSON output and caps what is kept (`redactSshErrorOutput`).
pub fn redact_ssh_error_output(output: &str) -> String {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let pattern = PATTERN.get_or_init(|| {
        regex::RegexBuilder::new(
            r#"("(?:access_token|bearerToken|credential|pairingToken|token)"\s*:\s*")[^"]+(")"#,
        )
        .case_insensitive(true)
        .build()
        .expect("redaction pattern")
    });
    let redacted = pattern
        .replace_all(output, "${1}[redacted]${2}")
        .into_owned();
    if redacted.encode_utf16().count() > MAX_SSH_ERROR_OUTPUT_LENGTH {
        let kept: String =
            char::decode_utf16(redacted.encode_utf16().take(MAX_SSH_ERROR_OUTPUT_LENGTH))
                .map(|c| c.unwrap_or('\u{fffd}'))
                .collect();
        format!("{kept}\n[truncated]")
    } else {
        redacted
    }
}

fn normalize_ssh_error_message(stdout: &str, stderr: &str, fallback: &str) -> String {
    let stderr = stderr.trim();
    if !stderr.is_empty() {
        return stderr.to_owned();
    }
    let stdout = stdout.trim();
    if stdout.is_empty() {
        fallback.to_owned()
    } else {
        stdout.to_owned()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SshError {
    #[error(transparent)]
    InvalidTarget(#[from] SshInvalidTarget),
    #[error("{message}")]
    Command {
        command: Vec<String>,
        exit_code: Option<i32>,
        stdout: String,
        stderr: String,
        message: String,
    },
}

#[derive(Debug, Clone, Default)]
pub struct RunSshOptions {
    pub pre_host_args: Vec<String>,
    pub remote_command_args: Vec<String>,
    pub stdin: Option<String>,
    pub timeout: Option<Duration>,
    /// "yes" by default (no prompts); callers that can prompt pass "no".
    pub batch_mode: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshCommandResult {
    pub stdout: String,
    pub stderr: String,
}

/// Runs ssh. `executable` is a command name or an absolute path.
#[derive(Debug, Clone)]
pub struct Ssh {
    pub executable: String,
}

impl Ssh {
    pub fn new(windows: bool) -> Self {
        Self {
            executable: ssh_command_for_platform(windows).to_owned(),
        }
    }

    pub fn with_executable(executable: impl Into<String>) -> Self {
        Self {
            executable: executable.into(),
        }
    }

    pub async fn run(
        &self,
        target: &SshTarget,
        options: RunSshOptions,
    ) -> Result<SshCommandResult, SshError> {
        let timeout = options.timeout.unwrap_or(DEFAULT_SSH_COMMAND_TIMEOUT);
        let host_spec = build_ssh_host_spec(target)?;
        let mut args = base_ssh_args(target, Some(options.batch_mode.as_deref().unwrap_or("yes")));
        args.extend(options.pre_host_args.iter().cloned());
        args.push(host_spec.clone());
        args.extend(options.remote_command_args.iter().cloned());
        let command_line: Vec<String> = std::iter::once("ssh".to_owned())
            .chain(args.iter().cloned())
            .collect();
        let failure = |message: String, exit_code: Option<i32>, stdout: String, stderr: String| {
            SshError::Command {
                command: command_line.clone(),
                exit_code,
                stdout,
                stderr,
                message,
            }
        };
        let work = async {
            let mut child = tokio::process::Command::new(&self.executable)
                .args(&args)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .map_err(|error| failure(error.to_string(), None, String::new(), String::new()))?;
            let mut stdin = child.stdin.take().expect("piped stdin");
            let input = options.stdin.clone();
            let feed = async move {
                if let Some(input) = input {
                    let _ = stdin.write_all(input.as_bytes()).await;
                }
                drop(stdin);
            };
            let mut stdout = child.stdout.take().expect("piped stdout");
            let mut stderr = child.stderr.take().expect("piped stderr");
            let (_, out, err, status) = tokio::join!(
                feed,
                async {
                    let mut buf = Vec::new();
                    stdout.read_to_end(&mut buf).await.map(|_| buf)
                },
                async {
                    let mut buf = Vec::new();
                    stderr.read_to_end(&mut buf).await.map(|_| buf)
                },
                child.wait()
            );
            let (Ok(out), Ok(err), Ok(status)) = (out, err, status) else {
                return Err(failure(
                    format!("Failed to run SSH command for {host_spec}."),
                    None,
                    String::new(),
                    String::new(),
                ));
            };
            Ok((
                String::from_utf8_lossy(&out).into_owned(),
                String::from_utf8_lossy(&err).into_owned(),
                status.code(),
            ))
        };
        let (stdout, stderr, code) = match tokio::time::timeout(timeout, work).await {
            Ok(result) => result?,
            Err(_) => {
                return Err(SshError::Command {
                    command: vec!["ssh".into()],
                    exit_code: None,
                    stdout: String::new(),
                    stderr: String::new(),
                    message: format!("SSH command timed out after {}ms.", timeout.as_millis()),
                });
            }
        };
        if code != Some(0) {
            let diagnostic_stdout = redact_ssh_error_output(&stdout);
            let message = normalize_ssh_error_message(
                &diagnostic_stdout,
                &stderr,
                &format!(
                    "SSH command failed for {host_spec} (exit {}).",
                    code.unwrap_or(-1)
                ),
            );
            return Err(failure(message, code, diagnostic_stdout, stderr));
        }
        Ok(SshCommandResult { stdout, stderr })
    }

    /// `ssh -G <alias>` into a target; any failure falls back to the alias itself.
    pub async fn resolve_target(&self, alias: &str) -> Result<SshTarget, SshInvalidTarget> {
        let alias = alias.trim();
        if alias.is_empty() {
            return Err(SshInvalidTarget {
                message: "SSH host alias is required.".into(),
            });
        }
        let fallback = SshTarget {
            alias: alias.to_owned(),
            hostname: alias.to_owned(),
            username: None,
            port: None,
        };
        let result = self
            .run(
                &fallback,
                RunSshOptions {
                    pre_host_args: vec!["-G".into()],
                    ..Default::default()
                },
            )
            .await;
        Ok(match result {
            Ok(output) => parse_ssh_resolve_output(alias, &output.stdout),
            Err(_) => fallback,
        })
    }
}

// ---- host discovery -------------------------------------------------------------------------

fn strip_inline_comment(line: &str) -> &str {
    line.split('#').next().unwrap_or("").trim()
}

fn split_directive_args(value: &str) -> Vec<String> {
    // `=` separates keyword and argument (`Host=foo`), except in `==`.
    let mut normalized = String::new();
    let chars: Vec<char> = value.chars().collect();
    for (index, c) in chars.iter().enumerate() {
        let next_is_eq = chars.get(index + 1) == Some(&'=');
        if *c == '=' && !next_is_eq {
            normalized.push(' ');
        } else {
            normalized.push(*c);
        }
    }
    normalized.split_whitespace().map(str::to_owned).collect()
}

fn expand_home_path(input: &str, home: &str) -> String {
    if input == "~" || input.starts_with("~/") || input.starts_with("~\\") {
        format!("{home}{}", &input[1..])
    } else {
        input.to_owned()
    }
}

/// `resolveSshConfigIncludePattern`: relative includes resolve against `<home>/.ssh`.
pub fn resolve_ssh_config_include_pattern(pattern: &str, home: &str) -> PathBuf {
    let expanded = expand_home_path(pattern, home);
    let path = Path::new(&expanded);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        Path::new(home).join(".ssh").join(path)
    }
}

fn has_ssh_pattern(value: &str) -> bool {
    value.contains('*') || value.contains('?') || value.starts_with('!')
}

fn glob_matches(pattern: &str, name: &str) -> bool {
    let mut regex = String::from("^");
    for c in pattern.chars() {
        match c {
            '*' => regex.push_str(".*"),
            '?' => regex.push('.'),
            other => regex.push_str(&regex::escape(&other.to_string())),
        }
    }
    regex.push('$');
    regex::Regex::new(&regex).is_ok_and(|re| re.is_match(name))
}

fn locale_compare(a: &str, b: &str) -> std::cmp::Ordering {
    // Built per call: sorting a handful of host names, and the collator is not `Sync`.
    icu_collator::Collator::try_new(Default::default(), Default::default())
        .expect("compiled ICU collation data")
        .compare(a, b)
}

fn expand_glob(pattern: &Path) -> Vec<PathBuf> {
    let text = pattern.to_string_lossy();
    if !text.contains('*') && !text.contains('?') {
        return if pattern.exists() {
            vec![pattern.to_path_buf()]
        } else {
            vec![]
        };
    }
    let (Some(directory), Some(base)) = (pattern.parent(), pattern.file_name()) else {
        return vec![];
    };
    let Ok(entries) = std::fs::read_dir(directory) else {
        return vec![];
    };
    let base = base.to_string_lossy();
    let mut matched: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|entry| glob_matches(&base, &entry.file_name().to_string_lossy()))
        .map(|entry| entry.path())
        .collect();
    matched.sort_by(|a, b| locale_compare(&a.to_string_lossy(), &b.to_string_lossy()));
    matched
}

fn collect_ssh_config_aliases(
    file: &Path,
    visited: &mut HashSet<PathBuf>,
    home: &str,
) -> BTreeSet<String> {
    let mut aliases = BTreeSet::new();
    let resolved = std::path::absolute(file).unwrap_or_else(|_| file.to_path_buf());
    if visited.contains(&resolved) || !resolved.exists() {
        return aliases;
    }
    visited.insert(resolved.clone());
    let Ok(raw) = std::fs::read_to_string(&resolved) else {
        return aliases;
    };
    for line in raw.lines() {
        let stripped = strip_inline_comment(line);
        if stripped.is_empty() {
            continue;
        }
        let mut parts = split_directive_args(stripped).into_iter();
        let directive = parts.next().unwrap_or_default().to_lowercase();
        let args: Vec<String> = parts.collect();
        if directive == "include" {
            for pattern in &args {
                for included in expand_glob(&resolve_ssh_config_include_pattern(pattern, home)) {
                    aliases.extend(collect_ssh_config_aliases(&included, visited, home));
                }
            }
        } else if directive == "host" {
            aliases.extend(
                args.into_iter()
                    .filter(|a| !a.is_empty() && !has_ssh_pattern(a)),
            );
        }
    }
    aliases
}

fn normalize_known_hosts_hostname(raw: &str) -> String {
    if let Some(caps) = regex::Regex::new(r"^\[([^\]]+)\]:(\d+)$")
        .expect("regex")
        .captures(raw)
    {
        return caps[1].to_owned();
    }
    if !raw.contains(':') {
        return raw.to_owned();
    }
    // A single colon is `host:port`; several are an IPv6 address.
    let first = raw.find(':');
    let last = raw.rfind(':');
    if first == last {
        raw[..last.unwrap()].to_owned()
    } else {
        raw.to_owned()
    }
}

/// Host names from a `known_hosts` file, without hashed entries or patterns.
pub fn parse_known_hosts_hostnames(raw: &str) -> Vec<String> {
    let mut hostnames = BTreeSet::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let without_marker = if trimmed.starts_with('@') {
            trimmed
                .split_whitespace()
                .skip(1)
                .collect::<Vec<_>>()
                .join(" ")
        } else {
            trimmed.to_owned()
        };
        let host_field = without_marker.split_whitespace().next().unwrap_or("");
        if host_field.is_empty() || host_field.starts_with('|') {
            continue;
        }
        for raw_host in host_field.split(',') {
            let host = normalize_known_hosts_hostname(raw_host).trim().to_owned();
            if !host.is_empty() && !has_ssh_pattern(&host) {
                hostnames.insert(host);
            }
        }
    }
    let mut sorted: Vec<String> = hostnames.into_iter().collect();
    sorted.sort_by(|a, b| locale_compare(a, b));
    sorted
}

/// Hosts from `~/.ssh/config` (following `Include`) and `~/.ssh/known_hosts`, ssh-config first
/// when a name is in both, sorted by alias.
pub fn discover_ssh_hosts(home_dir: &str) -> Vec<DiscoveredSshHost> {
    if home_dir.trim().is_empty() {
        return vec![];
    }
    let ssh_dir = Path::new(home_dir).join(".ssh");
    let config_aliases =
        collect_ssh_config_aliases(&ssh_dir.join("config"), &mut HashSet::new(), home_dir);
    let known_hosts = std::fs::read_to_string(ssh_dir.join("known_hosts"))
        .map(|raw| parse_known_hosts_hostnames(&raw))
        .unwrap_or_default();
    let mut discovered: BTreeMap<String, DiscoveredSshHost> = BTreeMap::new();
    for alias in config_aliases {
        discovered.insert(
            alias.clone(),
            DiscoveredSshHost {
                alias: alias.clone(),
                hostname: alias,
                username: None,
                port: None,
                source: HostSource::SshConfig,
            },
        );
    }
    for hostname in known_hosts {
        discovered
            .entry(hostname.clone())
            .or_insert(DiscoveredSshHost {
                alias: hostname.clone(),
                hostname,
                username: None,
                port: None,
                source: HostSource::KnownHosts,
            });
    }
    let mut hosts: Vec<DiscoveredSshHost> = discovered.into_values().collect();
    hosts.sort_by(|a, b| locale_compare(&a.alias, &b.alias));
    hosts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(alias: &str, hostname: &str, username: Option<&str>, port: Option<i64>) -> SshTarget {
        SshTarget {
            alias: alias.into(),
            hostname: hostname.into(),
            username: username.map(str::to_owned),
            port,
        }
    }

    #[test]
    fn parses_resolved_ssh_config_output() {
        let output = "hostname devbox.example.com\nuser julius\nport 2222\n";
        assert_eq!(
            parse_ssh_resolve_output("devbox", output),
            target("devbox", "devbox.example.com", Some("julius"), Some(2222))
        );
    }

    #[test]
    fn builds_interactive_args_without_forcing_batch_mode() {
        assert_eq!(
            base_ssh_args(
                &target("devbox", "devbox.example.com", Some("julius"), Some(2222)),
                Some("no")
            ),
            [
                "-o",
                "BatchMode=no",
                "-o",
                "ConnectTimeout=10",
                "-p",
                "2222"
            ]
        );
    }

    /// Remote servers store state under this key, so it must not change across releases.
    #[test]
    fn derives_a_stable_remote_state_key() {
        assert_eq!(
            remote_state_key(&target(
                "devbox",
                "devbox.example.com",
                Some("julius"),
                Some(2222)
            )),
            "711bc738002d72fd"
        );
        assert_eq!(
            remote_state_key(&target("fixture", "fixture", None, None)),
            "326264c4f08c8a0c"
        );
    }

    #[test]
    fn reads_the_last_non_empty_output_line() {
        assert_eq!(
            get_last_non_empty_output_line(
                "Welcome to the host\n\n{\"credential\":\"pairing-token\"}\n"
            )
            .as_deref(),
            Some("{\"credential\":\"pairing-token\"}")
        );
    }

    #[test]
    fn redacts_credentials_and_truncates() {
        let redacted = redact_ssh_error_output(r#"{"credential": "pairing-token", "other": "x"}"#);
        assert_eq!(redacted, r#"{"credential": "[redacted]", "other": "x"}"#);
        let long = redact_ssh_error_output(&"a".repeat(5_000));
        assert!(long.ends_with("\n[truncated]"));
        assert_eq!(long.len(), 4_000 + "\n[truncated]".len());
    }

    #[test]
    fn parses_known_hosts_without_hashed_hosts() {
        let raw = [
            "github.com ssh-ed25519 AAAA",
            "gitlab.com,gitlab-alias ssh-ed25519 BBBB",
            "|1|hashed|entry ssh-ed25519 CCCC",
            "@cert-authority *.example.com ssh-ed25519 DDDD",
            "[ssh.example.com]:2200 ssh-ed25519 EEEE",
            "port.example.com:22 ssh-ed25519 HHHH",
            "::1 ssh-ed25519 FFFF",
            "2001:db8::1 ssh-ed25519 GGGG",
            "",
        ]
        .join("\n");
        assert_eq!(
            parse_known_hosts_hostnames(&raw),
            [
                "::1",
                "2001:db8::1",
                "github.com",
                "gitlab-alias",
                "gitlab.com",
                "port.example.com",
                "ssh.example.com"
            ]
        );
    }

    #[test]
    fn expands_tilde_prefixed_include_patterns() {
        assert_eq!(
            resolve_ssh_config_include_pattern("~/.ssh/config.d/*.conf", "/tmp/home"),
            PathBuf::from("/tmp/home/.ssh/config.d/*.conf")
        );
        assert_eq!(
            resolve_ssh_config_include_pattern("config.d/*.conf", "/tmp/home"),
            PathBuf::from("/tmp/home/.ssh/config.d/*.conf")
        );
    }

    #[test]
    fn discovers_hosts_across_included_files() {
        let home = tempfile::Builder::new()
            .prefix("t3code-rust-port-test-ssh-")
            .tempdir()
            .unwrap();
        let ssh_dir = home.path().join(".ssh");
        std::fs::create_dir_all(ssh_dir.join("config.d")).unwrap();
        std::fs::write(
            ssh_dir.join("config"),
            "Host devbox\n  HostName devbox.example.com\nHost=equalsbox\nInclude=config.d/*.conf\n",
        )
        .unwrap();
        std::fs::write(
            ssh_dir.join("config.d").join("team.conf"),
            "Host staging\n  HostName staging.example.com\nHost *\n  ServerAliveInterval 30\n",
        )
        .unwrap();
        std::fs::write(
            ssh_dir.join("known_hosts"),
            "known.example.com ssh-ed25519 AAAA\n|1|hashed|entry ssh-ed25519 AAAA\n[bastion.example.com]:2222 ssh-ed25519 AAAA\n",
        )
        .unwrap();
        let hosts = discover_ssh_hosts(home.path().to_str().unwrap());
        let summary: Vec<(String, HostSource)> =
            hosts.iter().map(|h| (h.alias.clone(), h.source)).collect();
        assert_eq!(
            summary,
            [
                ("bastion.example.com".to_owned(), HostSource::KnownHosts),
                ("devbox".to_owned(), HostSource::SshConfig),
                ("equalsbox".to_owned(), HostSource::SshConfig),
                ("known.example.com".to_owned(), HostSource::KnownHosts),
                ("staging".to_owned(), HostSource::SshConfig),
            ]
        );
        assert!(
            hosts
                .iter()
                .all(|h| h.hostname == h.alias && h.username.is_none() && h.port.is_none())
        );
    }
}
