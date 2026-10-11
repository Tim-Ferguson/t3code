//! The pure half of the SSH launcher (`packages/ssh/src/tunnel.ts`): the shell scripts the desktop
//! app sends to a remote host over `ssh ... sh -s`, how their output is read back, and the
//! constants that size their budgets. Everything here is a function of its arguments; the process
//! and tunnel handling is in [`crate::ssh_tunnel`].
//!
//! # The scripts are byte-for-byte the original's
//!
//! They are security-sensitive (a remote shell runs them, one of them installs a binary) and the
//! remote compares them: the launch script embeds the runner script, and the remote restarts a
//! managed server whose runner changed (`cmp -s`). They live as template files in
//! `src/ssh_scripts/*.tmpl`, produced from the original's template literals after JavaScript
//! evaluated their escapes, and `tests/fixture_parity_ssh_tunnel.rs` compares every builder's
//! output with the original's, byte for byte.
//!
//! `node_engine_check.tmpl` is `satisfiesSemverRange.toString()` plus a call, taken from the
//! original run from source (type-stripped). A production bundle of the original may print the same
//! function with different whitespace; the remote only evaluates it, so the two behave alike.
//!
//! # Placeholder substitution keeps JavaScript's quirks
//!
//! `applyScriptPlaceholders` uses `String.prototype.replaceAll(token, value)`, whose replacement
//! string treats `$$`, `$&`, `` $` `` and `$'` specially. [`apply_script_placeholders`] does the same
//! (see [`js_replace_all`]).
//!
//! TODO(original packages/ssh/src/tunnel.ts `applyScriptPlaceholders`): because of that, the runner
//! script's `printf '%s\n' "$$" > "$T3_LOCK/pid.tmp"` becomes `"$"` whenever the runner is embedded as
//! a placeholder value, which is how both the launch script and the pairing script carry it. The
//! `run-t3.sh` the remote actually executes therefore writes the literal text `$` as the install
//! lock's owner pid; a second launch reads it, finds `kill -0 '$'` failing, and reclaims the lock as
//! stale while its owner is still installing. Only [`build_remote_t3_runner_script`] used on its own
//! (as the original's concurrency test does) keeps `"$$"`. Kept exactly; fixing it changes the bytes the remote
//! compares, so it needs a coordinated change with the original.

use std::sync::OnceLock;
use std::time::Duration;

use regex::Regex;
use serde_json::Value;

use crate::util::{js_number_to_string, js_trim, is_js_whitespace};

pub const DEFAULT_REMOTE_PORT: u32 = 3773;
pub const REMOTE_PORT_SCAN_WINDOW: u32 = 200;
pub const SSH_READY_TIMEOUT: Duration = Duration::from_millis(20_000);
pub const SSH_READY_PROBE_TIMEOUT: Duration = Duration::from_millis(1_000);
pub const TUNNEL_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(2_000);
pub const REMOTE_READY_TIMEOUT_MS: u32 = 60_000;
pub const REMOTE_LAUNCH_TIMEOUT: Duration = Duration::from_millis(90_000);
pub const REMOTE_ARCHIVE_CHECKSUMS_SECONDS: u32 = 30;
pub const REMOTE_ARCHIVE_DOWNLOAD_SECONDS: u32 = 240;
pub const REMOTE_ARCHIVE_LOCK_WAIT_SECONDS: u32 = 360;
pub const REMOTE_ARCHIVE_LAUNCH_TIMEOUT: Duration = Duration::from_millis(900_000);
pub const REMOTE_REUSE_READY_TIMEOUT_MS: u32 = 2_000;
/// How long `readRemoteServerLogTail` lets the remote `tail` run.
pub const REMOTE_LOG_TAIL_TIMEOUT: Duration = Duration::from_millis(10_000);

/// `cliRelease.ts`: where release archives are downloaded from when nothing overrides it.
pub const CLI_RELEASE_DEFAULT_BASE_URL: &str = "https://github.com/pingdotgg/t3code/releases/download";

/// `REMOTE_PICK_PORT_SCRIPT`, exported by the original (dev mode runs it with `node -`).
pub const REMOTE_PICK_PORT_SCRIPT: &str = include_str!("ssh_scripts/pick_port.tmpl");
/// The original's template (see the module docs).
pub const REMOTE_WAIT_READY_SCRIPT: &str = include_str!("ssh_scripts/wait_ready.tmpl");
/// The original's template (see the module docs).
pub const REMOTE_NODE_ENV_SCRIPT: &str = include_str!("ssh_scripts/node_env.tmpl");
/// The original's template (see the module docs).
pub const REMOTE_NODE_ENGINE_CHECK_SCRIPT: &str = include_str!("ssh_scripts/node_engine_check.tmpl");
/// The original's template (see the module docs).
pub const REMOTE_RUNNER_SCRIPT: &str = include_str!("ssh_scripts/runner.tmpl");
/// The original's template (see the module docs).
pub const REMOTE_LAUNCH_SCRIPT: &str = include_str!("ssh_scripts/launch.tmpl");
/// The original's template (see the module docs).
pub const REMOTE_PAIRING_SCRIPT: &str = include_str!("ssh_scripts/pairing.tmpl");
/// The original's template (see the module docs).
pub const REMOTE_STOP_SCRIPT: &str = include_str!("ssh_scripts/stop.tmpl");
/// The original's template (see the module docs).
pub const REMOTE_LOG_TAIL_SCRIPT: &str = include_str!("ssh_scripts/log_tail.tmpl");

/// `RemoteT3RunnerOptions`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteT3RunnerOptions {
    /// Dev mode: run `node <path>` on the remote instead of a release archive. The only mode that
    /// needs Node on the remote.
    pub node_script_path: Option<String>,
    pub node_engine_range: Option<String>,
    /// Exact version whose self-contained release archive the remote installs and runs. Required
    /// unless `node_script_path` is set; the remote then needs neither Node nor npm.
    pub archive_version: Option<String>,
    pub release_base_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RunnerScriptError {
    /// `SshMissingRunnerError`
    #[error("A remote t3 runner needs an archive version or a node script path.")]
    MissingRunner,
    /// `SshInvalidArchiveVersionError`
    #[error("'{archive_version}' is not an exact t3 version and cannot name a runtime directory.")]
    InvalidArchiveVersion { archive_version: String },
}

fn trimmed_or_empty(value: &Option<String>) -> &str {
    value.as_deref().map(js_trim).unwrap_or("")
}

/// `isNodeScriptRunner`
pub fn is_node_script_runner(runner: Option<&RemoteT3RunnerOptions>) -> bool {
    runner.is_some_and(|r| !trimmed_or_empty(&r.node_script_path).is_empty())
}

/// `sshRunnerLogFields`, as `(runner, detail)`.
pub fn ssh_runner_log_fields(
    runner: Option<&RemoteT3RunnerOptions>,
) -> (&'static str, Option<String>) {
    if let Some(runner) = runner {
        let node = trimmed_or_empty(&runner.node_script_path);
        if !node.is_empty() {
            return ("node-script", Some(node.to_owned()));
        }
        let archive = trimmed_or_empty(&runner.archive_version);
        if !archive.is_empty() {
            return ("archive", Some(archive.to_owned()));
        }
    }
    ("archive", None)
}

/// `stripTrailingNewlines`: `value.replace(/\n+$/u, "")`.
pub fn strip_trailing_newlines(value: &str) -> &str {
    value.trim_end_matches('\n')
}

/// `shellSingleQuote`
pub fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// `String.prototype.replaceAll(needle, replacement)` for a string needle: the replacement string
/// is expanded the way `GetSubstitution` does it with no capture groups (`$$`, `$&`, `` $` ``, `$'`;
/// every other `$` is literal).
pub fn js_replace_all(haystack: &str, needle: &str, replacement: &str) -> String {
    if needle.is_empty() {
        // Not reachable for placeholders; JavaScript would insert between every character.
        return haystack.to_owned();
    }
    let mut result = String::with_capacity(haystack.len());
    let mut cursor = 0;
    while let Some(found) = haystack[cursor..].find(needle) {
        let position = cursor + found;
        result.push_str(&haystack[cursor..position]);
        let mut chars = replacement.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '$' {
                result.push(c);
                continue;
            }
            match chars.peek() {
                Some('$') => {
                    chars.next();
                    result.push('$');
                }
                Some('&') => {
                    chars.next();
                    result.push_str(needle);
                }
                Some('`') => {
                    chars.next();
                    result.push_str(&haystack[..position]);
                }
                Some('\'') => {
                    chars.next();
                    result.push_str(&haystack[position + needle.len()..]);
                }
                _ => result.push('$'),
            }
        }
        cursor = position + needle.len();
    }
    result.push_str(&haystack[cursor..]);
    result
}

/// `applyScriptPlaceholders`: replaces `@@TOKEN@@` for each pair, in order.
///
/// TODO(original packages/ssh/src/tunnel.ts `applyScriptPlaceholders`): replacement values go through
/// JavaScript's `$` expansion (see the module docs for what that does to the embedded runner's
/// `"$$"`), and a value that itself contains `$&` expands to the placeholder token, so a dev
/// node-script path like `/tmp/a$&b/bin.mjs` renders differently from the path the caller gave.
/// Kept so the bytes stay identical to the original's.
pub fn apply_script_placeholders(template: &str, replacements: &[(&str, String)]) -> String {
    let mut result = template.to_owned();
    for (token, value) in replacements {
        result = js_replace_all(&result, &format!("@@{token}@@"), value);
    }
    result
}

/// The pattern that keeps an archive version a single directory name.
fn exact_archive_version() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(
            r"^(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$",
        )
        .expect("archive version pattern")
    })
}

/// `cliReleaseDownloadBaseUrl("", base).replace(/\/v$/u, "")`: the mirror (or GitHub) directory the
/// runner script appends `/v<version>/...` to.
fn release_base_url(input: Option<&str>) -> String {
    let base = input
        .map(js_trim)
        .filter(|b| !b.is_empty())
        .unwrap_or(CLI_RELEASE_DEFAULT_BASE_URL);
    let without_slashes = base.trim_end_matches('/');
    let with_version = format!("{without_slashes}/v");
    with_version
        .strip_suffix("/v")
        .unwrap_or(&with_version)
        .to_owned()
}

/// `buildRemoteT3RunnerScript`
pub fn build_remote_t3_runner_script(
    input: Option<&RemoteT3RunnerOptions>,
) -> Result<String, RunnerScriptError> {
    let empty = RemoteT3RunnerOptions::default();
    let options = input.unwrap_or(&empty);
    let node_script_path = trimmed_or_empty(&options.node_script_path);
    let archive_version = trimmed_or_empty(&options.archive_version);
    if node_script_path.is_empty() && archive_version.is_empty() {
        return Err(RunnerScriptError::MissingRunner);
    }
    if !archive_version.is_empty() && !exact_archive_version().is_match(archive_version) {
        return Err(RunnerScriptError::InvalidArchiveVersion {
            archive_version: archive_version.to_owned(),
        });
    }
    let release_base_url = release_base_url(options.release_base_url.as_deref());
    let rendered = apply_script_placeholders(
        REMOTE_RUNNER_SCRIPT,
        &[
            ("T3_NODE_SCRIPT_PATH", shell_single_quote(node_script_path)),
            ("T3_ARCHIVE_VERSION", shell_single_quote(archive_version)),
            ("T3_RELEASE_BASE_URL", shell_single_quote(&release_base_url)),
            (
                "T3_ARCHIVE_LOCK_WAIT_SECONDS",
                REMOTE_ARCHIVE_LOCK_WAIT_SECONDS.to_string(),
            ),
            (
                "T3_ARCHIVE_DOWNLOAD_SECONDS",
                REMOTE_ARCHIVE_DOWNLOAD_SECONDS.to_string(),
            ),
            (
                "T3_ARCHIVE_CHECKSUMS_SECONDS",
                REMOTE_ARCHIVE_CHECKSUMS_SECONDS.to_string(),
            ),
            ("T3_NODE_ENV_SCRIPT", build_remote_node_env_script(input)),
        ],
    );
    Ok(strip_trailing_newlines(&rendered).to_owned())
}

/// `buildRemoteNodeEnvScript`
pub fn build_remote_node_env_script(input: Option<&RemoteT3RunnerOptions>) -> String {
    let range = input
        .map(|o| trimmed_or_empty(&o.node_engine_range))
        .unwrap_or("");
    let rendered = apply_script_placeholders(
        REMOTE_NODE_ENV_SCRIPT,
        &[
            ("T3_NODE_ENGINE_RANGE", shell_single_quote(range)),
            (
                "T3_NODE_ENGINE_CHECK_SCRIPT",
                strip_trailing_newlines(REMOTE_NODE_ENGINE_CHECK_SCRIPT).to_owned(),
            ),
        ],
    );
    strip_trailing_newlines(&rendered).to_owned()
}

/// `buildRemoteLaunchScript`
pub fn build_remote_launch_script(
    input: Option<&RemoteT3RunnerOptions>,
) -> Result<String, RunnerScriptError> {
    // The original evaluates the placeholder values in order, and the runner is the only one that can
    // throw, so the error is the same wherever it is raised.
    let runner = build_remote_t3_runner_script(input)?;
    Ok(apply_script_placeholders(
        REMOTE_LAUNCH_SCRIPT,
        &[
            (
                "T3_ARCHIVE_MODE",
                if is_node_script_runner(input) { "0" } else { "1" }.to_owned(),
            ),
            ("T3_NODE_ENV_SCRIPT", build_remote_node_env_script(input)),
            (
                "T3_RUNNER_SCRIPT",
                strip_trailing_newlines(&runner).to_owned(),
            ),
            (
                "T3_PICK_PORT_SCRIPT",
                strip_trailing_newlines(REMOTE_PICK_PORT_SCRIPT).to_owned(),
            ),
            (
                "T3_WAIT_READY_SCRIPT",
                strip_trailing_newlines(REMOTE_WAIT_READY_SCRIPT).to_owned(),
            ),
            ("T3_DEFAULT_REMOTE_PORT", DEFAULT_REMOTE_PORT.to_string()),
            (
                "T3_REMOTE_PORT_SCAN_WINDOW",
                REMOTE_PORT_SCAN_WINDOW.to_string(),
            ),
            ("T3_READY_TIMEOUT_MS", REMOTE_READY_TIMEOUT_MS.to_string()),
            (
                "T3_REUSE_READY_TIMEOUT_MS",
                REMOTE_REUSE_READY_TIMEOUT_MS.to_string(),
            ),
            (
                "T3_READY_PROBE_TIMEOUT_MS",
                SSH_READY_PROBE_TIMEOUT.as_millis().to_string(),
            ),
        ],
    ))
}

/// `buildRemotePairingScript`
pub fn build_remote_pairing_script(
    state_key: &str,
    input: Option<&RemoteT3RunnerOptions>,
) -> Result<String, RunnerScriptError> {
    let runner = build_remote_t3_runner_script(input)?;
    Ok(apply_script_placeholders(
        REMOTE_PAIRING_SCRIPT,
        &[
            ("T3_STATE_KEY", state_key.to_owned()),
            (
                "T3_RUNNER_SCRIPT",
                strip_trailing_newlines(&runner).to_owned(),
            ),
        ],
    ))
}

/// `buildRemoteStopScript`
pub fn build_remote_stop_script(state_key: &str) -> String {
    apply_script_placeholders(REMOTE_STOP_SCRIPT, &[("T3_STATE_KEY", state_key.to_owned())])
}

/// `buildRemoteLogTailScript`
pub fn build_remote_log_tail_script(state_key: &str) -> String {
    apply_script_placeholders(
        REMOTE_LOG_TAIL_SCRIPT,
        &[("T3_STATE_KEY", state_key.to_owned())],
    )
}

// ---- reading the remote's answers -------------------------------------------------------------

/// `extractJsonObject` (`packages/shared/src/schemaJson.ts`): the first balanced `{...}` of
/// `raw`, or the trimmed text when there is none; an unbalanced object keeps everything after the
/// opening brace.
pub fn extract_json_object(raw: &str) -> String {
    let trimmed = js_trim(raw);
    if trimmed.is_empty() {
        return trimmed.to_owned();
    }
    let Some(start) = trimmed.find('{') else {
        return trimmed.to_owned();
    };
    let mut depth: i64 = 0;
    let mut in_string = false;
    let mut escaping = false;
    for (offset, c) in trimmed[start..].char_indices() {
        if in_string {
            if escaping {
                escaping = false;
            } else if c == '\\' {
                escaping = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return trimmed[start..start + offset + 1].to_owned();
                }
            }
            _ => {}
        }
    }
    trimmed[start..].to_owned()
}

/// The characters JavaScript's `\s` matches.
fn js_space_class() -> String {
    ('\0'..='\u{ffff}')
        .filter(|c| is_js_whitespace(*c))
        .map(|c| format!("\\u{{{:x}}}", c as u32))
        .collect()
}

/// `fromLenientJson`'s text step: strips `//` and `/* */` comments and trailing commas outside
/// string literals, then parses as JSON.
pub fn parse_lenient_json(input: &str) -> Result<Value, serde_json::Error> {
    static PATTERNS: OnceLock<[Regex; 3]> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        let space = js_space_class();
        let string = r#"("(?:[^"\\]|\\[^\n\r\u{2028}\u{2029}])*")"#;
        [
            Regex::new(&format!(r"{string}|//[^\n]*")).expect("line comment pattern"),
            Regex::new(&format!(r"{string}|/\*[\s\S]*?\*/")).expect("block comment pattern"),
            Regex::new(&format!(r"{string}|,([{space}]*[}}\]])")).expect("trailing comma pattern"),
        ]
    });
    let without_line = patterns[0].replace_all(input, |caps: &regex::Captures| {
        caps.get(1).map_or("", |m| m.as_str()).to_owned()
    });
    let without_block = patterns[1].replace_all(&without_line, |caps: &regex::Captures| {
        caps.get(1).map_or("", |m| m.as_str()).to_owned()
    });
    // Group 1 is a string literal (kept as it is), group 2 the bracket after a trailing comma.
    let stripped = patterns[2].replace_all(&without_block, |caps: &regex::Captures| {
        match (caps.get(1), caps.get(2)) {
            (Some(string), _) => string.as_str().to_owned(),
            (None, Some(bracket)) => bracket.as_str().to_owned(),
            (None, None) => String::new(),
        }
    });
    serde_json::from_str(&stripped)
}

/// `RemoteLaunchResult.serverKind`
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RemoteServerKind {
    External,
    Managed,
}

/// `RemoteLaunchResult`; `remote_port` is a JSON number, which may be fractional.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteLaunchResult {
    pub remote_port: f64,
    pub server_kind: Option<RemoteServerKind>,
}

fn decode_launch_value(value: &Value) -> Option<RemoteLaunchResult> {
    let object = value.as_object()?;
    let remote_port = object.get("remotePort")?.as_f64()?;
    let server_kind = match object.get("serverKind") {
        None => None,
        Some(Value::String(kind)) if kind == "external" => Some(RemoteServerKind::External),
        Some(Value::String(kind)) if kind == "managed" => Some(RemoteServerKind::Managed),
        Some(_) => return None,
    };
    Some(RemoteLaunchResult {
        remote_port,
        server_kind,
    })
}

fn decode_pairing_value(value: &Value) -> Option<String> {
    value
        .as_object()?
        .get("credential")?
        .as_str()
        .map(str::to_owned)
}

/// `decodeRemoteJsonOutput`: decode the whole output; failing that, decode the first `{...}` in it
/// (shell start-up noise before the JSON, or pretty-printed JSON followed by blank lines).
fn decode_remote_json_output<T>(stdout: &str, decode: impl Fn(&Value) -> Option<T>) -> Option<T> {
    let attempt = |text: &str| parse_lenient_json(text).ok().and_then(|v| decode(&v));
    if let Some(decoded) = attempt(stdout) {
        return Some(decoded);
    }
    let json_object = extract_json_object(stdout);
    if json_object == js_trim(stdout) {
        return None;
    }
    attempt(&json_object)
}

/// `decodeRemoteLaunchOutput`; `None` is a decode failure.
pub fn decode_remote_launch_output(stdout: &str) -> Option<RemoteLaunchResult> {
    decode_remote_json_output(stdout, decode_launch_value)
}

/// `decodeRemotePairingOutput`; `None` is a decode failure.
pub fn decode_remote_pairing_output(stdout: &str) -> Option<String> {
    decode_remote_json_output(stdout, decode_pairing_value)
}

/// `Number.isInteger` for a JSON number, as the integer it names. Integers beyond `i64` cannot be
/// ports; they are reported through the same "invalid remote port" error as fractions.
pub fn json_integer(value: f64) -> Option<i64> {
    (value.is_finite() && value.fract() == 0.0 && value.abs() < 9.0e18).then_some(value as i64)
}

/// The text `String(remotePort)` gives in the launch error message.
pub fn describe_remote_port(value: f64) -> String {
    js_number_to_string(value)
}

/// `satisfiesSemverRange` (`packages/shared/src/semver.ts`): the small range checker the remote
/// runs to decide whether its `node` is usable (dev mode only). A port of the same function the
/// remote receives as text in `node_engine_check.tmpl`.
pub fn satisfies_semver_range(raw_version: &str, range: &str) -> bool {
    static VERSION: OnceLock<Regex> = OnceLock::new();
    static COMPARATOR: OnceLock<Regex> = OnceLock::new();
    static TARGET: OnceLock<Regex> = OnceLock::new();
    let version_pattern = VERSION.get_or_init(|| {
        Regex::new(r"^(\d+)(?:\.(\d+))?(?:\.(\d+))?(?:-[0-9A-Za-z.-]+)?$").expect("version")
    });
    let comparator_pattern = COMPARATOR.get_or_init(|| {
        Regex::new(r"^(\^|>=|>|<=|<|=)?\s*v?(\d+(?:\.\d+){0,2})$").expect("comparator")
    });
    let target_pattern = TARGET
        .get_or_init(|| Regex::new(r"^(\d+)(?:\.(\d+))?(?:\.(\d+))?$").expect("target"));

    let trimmed = js_trim(raw_version);
    let normalized = trimmed.strip_prefix('v').unwrap_or(trimmed);
    let Some(version_match) = version_pattern.captures(normalized) else {
        return false;
    };
    // `Number("")` is 0, and `Number(digits)` loses precision beyond 2^53 just as f64 does.
    let number = |m: Option<regex::Match>| m.map_or(0.0, |m| m.as_str().parse::<f64>().unwrap_or(0.0));
    let version = (
        number(version_match.get(1)),
        number(version_match.get(2)),
        number(version_match.get(3)),
    );

    range.split("||").any(|group| {
        let comparators: Vec<&str> = js_trim(group)
            .split(is_js_whitespace)
            .filter(|c| !c.is_empty())
            .collect();
        if comparators.is_empty() {
            return false;
        }
        comparators.iter().all(|comparator| {
            let Some(matched) = comparator_pattern.captures(js_trim(comparator)) else {
                return false;
            };
            let Some(target_text) = matched.get(2) else {
                return false;
            };
            let Some(target_match) = target_pattern.captures(target_text.as_str()) else {
                return false;
            };
            let target = (
                number(target_match.get(1)),
                number(target_match.get(2)),
                number(target_match.get(3)),
            );
            let compared: i32 = if version.0 != target.0 {
                if version.0 > target.0 { 1 } else { -1 }
            } else if version.1 != target.1 {
                if version.1 > target.1 { 1 } else { -1 }
            } else if version.2 != target.2 {
                if version.2 > target.2 { 1 } else { -1 }
            } else {
                0
            };
            match matched.get(1).map_or("=", |m| m.as_str()) {
                "^" => {
                    if compared < 0 {
                        false
                    } else if target.0 > 0.0 {
                        version.0 == target.0
                    } else if target.1 > 0.0 {
                        version.0 == 0.0 && version.1 == target.1
                    } else {
                        version.0 == 0.0 && version.1 == 0.0 && version.2 == target.2
                    }
                }
                ">=" => compared >= 0,
                ">" => compared > 0,
                "<=" => compared <= 0,
                "<" => compared < 0,
                "=" => compared == 0,
                _ => false,
            }
        })
    })
}

/// `isLoopbackHostname`
fn is_loopback_hostname(hostname: &str) -> bool {
    let lowered = js_trim(hostname).to_lowercase();
    let normalized = lowered
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(&lowered);
    matches!(normalized, "127.0.0.1" | "::1" | "localhost")
}

/// `resolveLoopbackSshHttpBaseUrl`: the desktop only talks to forwarded loopback URLs. Failures are
/// the original's `SshHttpBridgeError` messages.
pub fn resolve_loopback_ssh_http_base_url(raw: &str) -> Result<String, String> {
    if js_trim(raw).is_empty() {
        return Err("Invalid SSH forwarded http base URL.".to_owned());
    }
    let url = url::Url::parse(raw).map_err(|_| "Invalid URL".to_owned())?;
    // `URL.hostname` keeps the brackets of an IPv6 address; `url::Url::host_str` does too.
    let host = url.host_str().unwrap_or("");
    if !is_loopback_hostname(host) {
        return Err("SSH desktop bridge only supports loopback forwarded URLs.".to_owned());
    }
    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacement_strings_expand_like_javascript_replace_all() {
        assert_eq!(js_replace_all("a@@X@@b", "@@X@@", "$$"), "a$b");
        assert_eq!(js_replace_all("a@@X@@b", "@@X@@", "[$&]"), "a[@@X@@]b");
        assert_eq!(js_replace_all("a@@X@@b", "@@X@@", "[$`]"), "a[a]b");
        assert_eq!(js_replace_all("a@@X@@b", "@@X@@", "[$']"), "a[b]b");
        // Anything else after a dollar sign is literal, including capture-style references.
        assert_eq!(js_replace_all("@@X@@", "@@X@@", "$1 $<n> $ $x $"), "$1 $<n> $ $x $");
        // Every occurrence is replaced, from the original text.
        assert_eq!(js_replace_all("@@X@@-@@X@@", "@@X@@", "$`"), "-@@X@@-");
    }

    #[test]
    fn placeholders_apply_in_order_so_later_ones_see_earlier_values() {
        let rendered = apply_script_placeholders(
            "@@A@@ @@B@@",
            &[("A", "@@B@@!".to_owned()), ("B", "b".to_owned())],
        );
        assert_eq!(rendered, "b! b");
    }

    #[test]
    fn shell_quoting_closes_and_reopens_around_single_quotes() {
        assert_eq!(shell_single_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_single_quote(""), "''");
    }

    #[test]
    fn lenient_json_strips_comments_and_trailing_commas_outside_strings() {
        let parsed = parse_lenient_json(
            "// c\n{\"a\": \"x // not a comment\", /* b */ \"b\": [1, 2,], \"c\": \"a,]\",}",
        )
        .unwrap();
        assert_eq!(
            parsed,
            serde_json::json!({"a": "x // not a comment", "b": [1, 2], "c": "a,]"})
        );
        assert!(parse_lenient_json("{").is_err());
    }

    #[test]
    fn the_range_checker_matches_the_documented_cases() {
        let range = "^22.16 || ^23.11 || >=24.10";
        for (version, expected) in [
            ("22.16.0", true),
            ("23.11.1", true),
            ("24.10.0", true),
            ("22.15.9", false),
            ("23.10.9", false),
            ("24.9.9", false),
            ("v24.10.0", true),
            ("not-a-version", false),
        ] {
            assert_eq!(satisfies_semver_range(version, range), expected, "{version}");
        }
        assert!(satisfies_semver_range("24.9.0", ">=24.0 <24.10"));
        assert!(!satisfies_semver_range("24.10.0", ">=24.0 <24.10"));
        assert!(!satisfies_semver_range("24.10.0", "~24.10"));
        assert!(satisfies_semver_range("0.2.9", "^0.2.3") && !satisfies_semver_range("0.3.0", "^0.2.3"));
    }
}
