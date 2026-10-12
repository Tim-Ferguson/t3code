//! The commands of `t3` besides the server itself that this binary understands (`apps/server/src/cli/*.ts`, `bin.ts`):
//! `--version`, `pair`, `auth pairing|session ...`, `project add|remove|rename`, and the hidden `__ssh-helper` and
//! `__service-preflight` the remote SSH launch and the staged-update check run. `main.rs` hands the arguments here first; a
//! command this module does not know is the server's own (`serve`, `import`, `connect`, ...).
//!
//! Every command opens the state directory the same way `serve` does (`<home>/userdata`, selected by `--base-dir`, else
//! `--state-dir`, else `T3CODE_HOME`, else `.t3-rust` under the current directory like `serve`) and prints exactly what the
//! original prints, including the blank line `Console.log` adds after text that already ends with a newline. A failure
//! prints its message on standard error and exits 1.
use crate::{
    auth::{AuthService, IssuedPairingLink},
    persistence::Store,
    project::ProjectService,
};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};
use t3_contracts::AuthEnvironmentScope;
use t3_remote::tailscale_pair::{EnvironmentProbeResult, EnvironmentProber, PairError, PairServerState, PairTailscale, PairTarget};

/// `Command.make("t3")`'s name, as `--version` prints it.
const NAME: &str = "t3";
const DEFAULT_SESSION_SUBJECT: &str = "cli-issued-session";
const DEFAULT_PAIRING_TTL_MINUTES: i64 = 5;
const DEFAULT_SESSION_TTL_DAYS: i64 = 30;

// ---------------------------------------------------------------------------------------------
// Flags
// ---------------------------------------------------------------------------------------------

/// The flags a command takes: those with a value, those without, and those that may repeat.
pub(crate) struct Spec {
    pub(crate) values: &'static [&'static str],
    pub(crate) booleans: &'static [&'static str],
    pub(crate) repeated: &'static [&'static str],
    /// How many positional arguments it takes (`min`, `max`).
    pub(crate) positionals: (usize, usize),
}

#[derive(Default, Debug)]
pub(crate) struct Parsed {
    values: HashMap<String, Vec<String>>,
    booleans: HashMap<String, bool>,
    pub(crate) positionals: Vec<String>,
}

impl Parsed {
    pub(crate) fn value(&self, name: &str) -> Option<&str> {
        self.values.get(name).and_then(|values| values.last()).map(String::as_str)
    }
    fn all(&self, name: &str) -> Vec<&str> {
        self.values.get(name).map(|values| values.iter().map(String::as_str).collect()).unwrap_or_default()
    }
    fn flag(&self, name: &str) -> bool {
        self.booleans.get(name).copied().unwrap_or(false)
    }
}

pub(crate) fn parse(args: &[String], spec: &Spec) -> Result<Parsed, String> {
    let mut parsed = Parsed::default();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        index += 1;
        let Some(flag) = arg.strip_prefix("--") else {
            parsed.positionals.push(arg.clone());
            continue;
        };
        let (name, inline) = match flag.split_once('=') {
            Some((name, value)) => (name, Some(value.to_owned())),
            None => (flag, None),
        };
        if spec.booleans.contains(&name) {
            let value = match inline.as_deref() {
                None | Some("true") => true,
                Some("false") => false,
                Some(other) => return Err(format!("Invalid value for flag --{name}: \"{other}\". Expected: true or false")),
            };
            parsed.booleans.insert(name.to_owned(), value);
        } else if spec.values.contains(&name) {
            let value = match inline {
                Some(value) => value,
                None => {
                    let value = args.get(index).cloned().ok_or_else(|| format!("Flag --{name} requires a value"))?;
                    index += 1;
                    value
                }
            };
            let slot = parsed.values.entry(name.to_owned()).or_default();
            if !slot.is_empty() && !spec.repeated.contains(&name) {
                return Err(format!("Flag --{name} may be given only once"));
            }
            slot.push(value);
        } else {
            return Err(format!("Unrecognized flag: --{name}"));
        }
    }
    let (minimum, maximum) = spec.positionals;
    if parsed.positionals.len() < minimum {
        return Err("Missing required argument".into());
    }
    if parsed.positionals.len() > maximum {
        return Err(format!("Unexpected argument: {}", parsed.positionals[maximum]));
    }
    Ok(parsed)
}

/// What a command prints and how it ends.
pub struct Outcome {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}

impl Outcome {
    /// `Console.log`: the text and a newline after it.
    pub(crate) fn log(text: impl Into<String>) -> Self {
        Self { stdout: format!("{}\n", text.into()), stderr: String::new(), code: 0 }
    }
    pub(crate) fn failure(message: impl Into<String>) -> Self {
        Self { stdout: String::new(), stderr: format!("{}\n", message.into()), code: 1 }
    }
}

// ---------------------------------------------------------------------------------------------
// Where the state is
// ---------------------------------------------------------------------------------------------

/// `expandHomePath` and `resolveBaseDir`'s reading of a path: `~` is the user's home.
pub fn expand_home(input: &str, env: &dyn Fn(&str) -> Option<String>) -> PathBuf {
    let home = || env("HOME").or_else(|| env("USERPROFILE")).map(PathBuf::from).unwrap_or_default();
    if input == "~" {
        return home();
    }
    if let Some(rest) = input.strip_prefix("~/").or_else(|| input.strip_prefix("~\\")) {
        return home().join(rest);
    }
    PathBuf::from(input)
}

/// The state directory `serve` would use for the same flags: `--base-dir` (its `userdata`), else `--state-dir`, else
/// `T3CODE_HOME`'s `userdata`, else `.t3-rust` under the current directory.
pub(crate) fn state_dir(parsed: &Parsed, env: &dyn Fn(&str) -> Option<String>) -> Result<PathBuf, String> {
    let absolute = |path: PathBuf| -> Result<PathBuf, String> {
        if path.is_absolute() { Ok(path) } else { Ok(std::env::current_dir().map_err(|error| error.to_string())?.join(path)) }
    };
    if let Some(base) = parsed.value("base-dir").filter(|base| !base.trim().is_empty()) {
        return absolute(expand_home(base.trim(), env).join("userdata"));
    }
    if let Some(directory) = parsed.value("state-dir") {
        return absolute(PathBuf::from(directory));
    }
    if let Some(home) = env("T3CODE_HOME").filter(|home| !home.trim().is_empty()) {
        return absolute(expand_home(home.trim(), env).join("userdata"));
    }
    Ok(std::env::current_dir().map_err(|error| error.to_string())?.join(".t3-rust"))
}

const LOCATION: &[&str] = &["base-dir", "state-dir"];

/// The signing secret of the state directory (`rust-signing-secret`), made when it is not there yet.
pub fn signing_secret(path: &Path) -> Result<[u8; 32], Box<dyn std::error::Error>> {
    match std::fs::read(path) {
        Ok(bytes) => return bytes.try_into().map_err(|_| "Invalid native signing-secret length".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let bytes = rand::random::<[u8; 32]>();
    publish_once(path, &bytes)?;
    std::fs::read(path)?.try_into().map_err(|_| "Invalid native signing-secret length".into())
}

/// Publish a fully synced inode using an atomic, exclusive hard link. Concurrent serve/pair invocations observe the same
/// complete identity and secret.
pub fn publish_once(path: &Path, bytes: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;
    if path.exists() {
        return Ok(());
    }
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> std::io::Result<()> {
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        match std::fs::hard_link(&temporary, path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(error),
        }
    })();
    let _ = std::fs::remove_file(temporary);
    result?;
    Ok(())
}

/// The state of a server, opened for a command that reads or writes access or projects. A directory that holds the original's
/// database and no store of this server is refused: opening a store here would stop the first start from importing it.
fn open_state(state_dir: &Path) -> Result<(Store, AuthService), String> {
    let database = state_dir.join("rust-state.sqlite");
    if !database.exists() && (state_dir.join("statev2.sqlite").exists() || state_dir.join("state.sqlite").exists()) {
        return Err(format!("{} holds the original server's state and none of this server's. Start the server there once first.", state_dir.display()));
    }
    std::fs::create_dir_all(state_dir).map_err(|error| error.to_string())?;
    let store = Store::open(&database).map_err(|error| error.to_string())?;
    let secret = signing_secret(&state_dir.join("rust-signing-secret")).map_err(|error| error.to_string())?;
    let auth = AuthService::new(store.clone(), secret, "t3_session".into(), "loopback-browser".into()).map_err(|error| error.to_string())?;
    Ok((store, auth))
}

// ---------------------------------------------------------------------------------------------
// Values the flags take
// ---------------------------------------------------------------------------------------------

/// `DurationFromString`: `5m`, `1h`, `30d`, `15 minutes`.
pub fn parse_duration(input: &str) -> Option<Duration> {
    let trimmed = input.trim();
    let shorthand = regex::Regex::new(r"(?i)^(\d+)(ms|s|m|h|d|w)$").expect("a pattern");
    let (amount, unit) = if let Some(captures) = shorthand.captures(trimmed) {
        (captures[1].parse::<i64>().ok()?, match captures[2].to_lowercase().as_str() {
            "ms" => "millis",
            "s" => "seconds",
            "m" => "minutes",
            "h" => "hours",
            "d" => "days",
            _ => "weeks",
        })
    } else {
        let spelled = regex::Regex::new(r"^(\d+)\s+(millis?|seconds?|minutes?|hours?|days?|weeks?)$").expect("a pattern");
        let captures = spelled.captures(trimmed)?;
        let unit = captures[2].to_owned();
        let unit = if unit.starts_with("milli") {
            "millis"
        } else if unit.starts_with("second") {
            "seconds"
        } else if unit.starts_with("minute") {
            "minutes"
        } else if unit.starts_with("hour") {
            "hours"
        } else if unit.starts_with("day") {
            "days"
        } else {
            "weeks"
        };
        (captures[1].parse::<i64>().ok()?, unit)
    };
    let milliseconds = match unit {
        "millis" => amount,
        "seconds" => amount.checked_mul(1_000)?,
        "minutes" => amount.checked_mul(60_000)?,
        "hours" => amount.checked_mul(3_600_000)?,
        "days" => amount.checked_mul(86_400_000)?,
        _ => amount.checked_mul(604_800_000)?,
    };
    Duration::try_milliseconds(milliseconds)
}

fn ttl(parsed: &Parsed, default: Duration) -> Result<Duration, String> {
    match parsed.value("ttl") {
        None => Ok(default),
        Some(text) => parse_duration(text).filter(|ttl| *ttl > Duration::zero()).ok_or_else(|| format!("Invalid value for flag --ttl: \"{text}\". Invalid duration. Use values like 5m, 1h, 30d, or 15 minutes.")),
    }
}

/// `authScopesFlag`: the scopes named, once each, else the defaults.
fn scopes(parsed: &Parsed, defaults: &[AuthEnvironmentScope]) -> Result<Vec<AuthEnvironmentScope>, String> {
    let mut chosen: Vec<AuthEnvironmentScope> = Vec::new();
    for name in parsed.all("scope") {
        let scope: AuthEnvironmentScope = serde_json::from_value(json!(name)).ok().filter(|scope: &AuthEnvironmentScope| scope.is_grantable()).ok_or_else(|| {
            let known: Vec<String> = ALL_SCOPES.iter().filter(|scope| scope.is_grantable()).map(|scope| scope_name(*scope)).collect();
            format!("Invalid value for flag --scope: \"{name}\". Expected one of: {}", known.join(", "))
        })?;
        if !chosen.contains(&scope) {
            chosen.push(scope);
        }
    }
    Ok(if chosen.is_empty() { defaults.to_vec() } else { chosen })
}

const ALL_SCOPES: [AuthEnvironmentScope; 17] = {
    use AuthEnvironmentScope::*;
    [OrchestrationRead, OrchestrationOperate, SettingsWrite, ProvidersManage, EnvironmentMaintain, PreviewOperate, DiagnosticsRead, TerminalRead, TerminalOperate, SourceControlWrite, FilesystemRead, FilesystemWrite, ReviewWrite, AccessRead, AccessWrite, RelayRead, RelayWrite]
};

fn scope_name(scope: AuthEnvironmentScope) -> String {
    serde_json::to_value(scope).ok().and_then(|value| value.as_str().map(str::to_owned)).unwrap_or_default()
}

fn scope_names(scopes: &[AuthEnvironmentScope]) -> Vec<String> {
    scopes.iter().map(|scope| scope_name(*scope)).collect()
}

pub(crate) fn iso(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Millis, true)
}

// ---------------------------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------------------------

/// Runs the command the arguments name, or `None` when they name the server's own.
pub fn dispatch(args: &[String], env: &dyn Fn(&str) -> Option<String>) -> Option<Outcome> {
    let first = args.first().map(String::as_str)?;
    let rest = &args[1..];
    match first {
        "--version" | "-v" => Some(Outcome::log(format!("{NAME} v{}", env!("CARGO_PKG_VERSION")))),
        "__service-preflight" => Some(service_preflight(rest)),
        "__ssh-helper" => Some(ssh_helper(rest)),
        "auth" => Some(auth_command(rest, env)),
        "project" => Some(project_command(rest, env)),
        "pair" => Some(pair_command(rest, env)),
        "theme" => Some(crate::cli_theme::theme_command(rest, env)),
        "browser" => Some(crate::cli_browser::browser_command(rest, env)),
        _ => None,
    }
}

fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_multi_thread().enable_all().build().map_err(|error| error.to_string())
}

/// The hidden check a staged update runs before it takes over: one JSON line, `ready` or `blocked`.
fn service_preflight(args: &[String]) -> Outcome {
    const SPEC: Spec = Spec { values: &["database-path", "launcher-protocol"], booleans: &[], repeated: &[], positionals: (0, 0) };
    let run = || -> Result<Outcome, String> {
        let parsed = parse(args, &SPEC)?;
        let path = parsed.value("database-path").ok_or("Missing required flag: --database-path")?;
        let protocol = parsed.value("launcher-protocol").ok_or("Missing required flag: --launcher-protocol")?;
        let protocol: u32 = protocol.parse().map_err(|_| format!("Invalid value for flag --launcher-protocol: \"{protocol}\". Expected an integer"))?;
        Ok(Outcome::log(t3_remote::service_preflight::run_service_preflight(path, protocol, env!("CARGO_PKG_VERSION")).to_json().to_string()))
    };
    run().unwrap_or_else(Outcome::failure)
}

/// The hidden helpers the SSH launch script runs on the remote host; the script parses their output, so nothing here is
/// decorated: `pick-port` and `runtime-port` write no newline, and their exit codes carry the answer.
fn ssh_helper(args: &[String]) -> Outcome {
    match runtime() {
        Ok(runtime) => {
            let output = runtime.block_on(t3_remote::ssh_helper::run(args));
            Outcome { stdout: output.stdout, stderr: String::new(), code: output.exit_code }
        }
        Err(error) => Outcome::failure(error),
    }
}

// ---------------------------------------------------------------------------------------------
// auth
// ---------------------------------------------------------------------------------------------

const AUTH_LOCATION: &[&str] = &["base-dir", "state-dir"];

fn auth_command(args: &[String], env: &dyn Fn(&str) -> Option<String>) -> Outcome {
    let (group, rest) = match args.split_first() {
        Some((group, rest)) if matches!(group.as_str(), "pairing" | "session") => (group.as_str(), rest),
        _ => return Outcome::failure("Usage: auth <pairing|session> ..."),
    };
    let (action, rest) = match rest.split_first() {
        Some((action, rest)) => (action.as_str(), rest),
        None => return Outcome::failure(format!("Usage: auth {group} <{}> ...", if group == "pairing" { "create|list|revoke" } else { "issue|list|revoke" })),
    };
    let now = Utc::now();
    let result = match (group, action) {
        ("pairing", "create") => pairing_create(rest, env, now),
        ("pairing", "list") => pairing_list(rest, env, now),
        ("pairing", "revoke") => pairing_revoke(rest, env, now),
        ("session", "issue") => session_issue(rest, env, now),
        ("session", "list") => session_list(rest, env, now),
        ("session", "revoke") => session_revoke(rest, env, now),
        _ => Err(format!("Unknown command: auth {group} {action}")),
    };
    result.unwrap_or_else(Outcome::failure)
}

fn pairing_create(args: &[String], env: &dyn Fn(&str) -> Option<String>, now: DateTime<Utc>) -> Result<Outcome, String> {
    const SPEC: Spec = Spec { values: &["base-dir", "state-dir", "scope", "ttl", "label", "base-url"], booleans: &["json"], repeated: &["scope"], positionals: (0, 0) };
    let parsed = parse(args, &SPEC)?;
    let scopes = scopes(&parsed, t3_contracts::AUTH_STANDARD_CLIENT_SCOPES)?;
    let ttl = ttl(&parsed, Duration::minutes(DEFAULT_PAIRING_TTL_MINUTES))?;
    let (_, auth) = open_state(&state_dir(&parsed, env)?)?;
    let issued = auth.issue_pairing_link(&scopes, "one-time-token", parsed.value("label"), now, ttl).map_err(|error| error.to_string())?;
    Ok(Outcome::log(format_issued_pairing_credential(&issued, &scopes, parsed.flag("json"), parsed.value("base-url"))?))
}

/// `buildPairingUrl` / the `/pair` URL `formatIssuedPairingCredential` makes: the token rides in the fragment.
pub fn build_pairing_url(base: &str, token: &str) -> Result<String, String> {
    let mut url = url::Url::parse(base).map_err(|_| "Invalid URL".to_owned())?;
    url.set_path("/pair");
    let kept: Vec<(String, String)> = url.query_pairs().filter(|(name, _)| name != "token").map(|(name, value)| (name.into_owned(), value.into_owned())).collect();
    if kept.is_empty() {
        url.set_query(None);
    } else {
        url.query_pairs_mut().clear().extend_pairs(kept);
    }
    url.set_fragment(Some(&url::form_urlencoded::Serializer::new(String::new()).append_pair("token", token).finish()));
    Ok(url.to_string())
}

/// `formatIssuedPairingCredential`.
pub fn format_issued_pairing_credential(issued: &IssuedPairingLink, scopes: &[AuthEnvironmentScope], json_output: bool, base_url: Option<&str>) -> Result<String, String> {
    let pair_url = match base_url.filter(|base| !base.is_empty()) {
        Some(base) => {
            // `new URL("/pair", base)`: resolving an absolute path drops the base's query and fragment (`buildPairingUrl`, which
            // sets the pathname instead, keeps the query).
            let joined = url::Url::parse(base).and_then(|base| base.join("/pair")).map_err(|_| "Invalid URL".to_owned())?;
            Some(build_pairing_url(joined.as_str(), &issued.credential)?)
        }
        None => None,
    };
    if json_output {
        let mut output = json!({"id": issued.id, "credential": issued.credential});
        if let Some(label) = issued.label.as_deref().filter(|label| !label.is_empty()) {
            output["label"] = json!(label);
        }
        output["scopes"] = json!(scope_names(scopes));
        output["expiresAt"] = json!(iso(issued.expires_at));
        if let Some(url) = pair_url {
            output["pairUrl"] = json!(url);
        }
        return Ok(format!("{}\n", serde_json::to_string_pretty(&output).expect("a value encodes")));
    }
    let mut lines = vec![format!("Issued client pairing token {}.", issued.id), format!("Token: {}", issued.credential)];
    if let Some(url) = pair_url {
        lines.push(format!("Pair URL: {url}"));
    }
    // TODO(original apps/server/src/cliAuthFormat.ts `formatIssuedPairingCredential`): the expiry is interpolated as a
    // DateTime object, so the text reads `DateTime.Utc(<iso>)` where the JSON reads the bare time. Copied.
    lines.push(format!("Expires at: DateTime.Utc({})", iso(issued.expires_at)));
    Ok(format!("{}\n", lines.join("\n")))
}

fn pairing_list(args: &[String], env: &dyn Fn(&str) -> Option<String>, now: DateTime<Utc>) -> Result<Outcome, String> {
    const SPEC: Spec = Spec { values: AUTH_LOCATION, booleans: &["json"], repeated: &[], positionals: (0, 0) };
    let parsed = parse(args, &SPEC)?;
    let (_, auth) = open_state(&state_dir(&parsed, env)?)?;
    let links = auth.list_pairing_links(now).map_err(|error| error.to_string())?;
    let rows: Vec<Value> = links.iter().map(|link| serde_json::to_value(link).expect("a link encodes")).collect();
    Ok(Outcome::log(format_pairing_credential_list(&rows, parsed.flag("json"))))
}

/// `formatPairingCredentialList`, over the links as the wire carries them.
pub fn format_pairing_credential_list(rows: &[Value], json_output: bool) -> String {
    let scopes_of = |row: &Value| row.get("permissions").or_else(|| row.get("scopes")).cloned().unwrap_or(json!([]));
    if json_output {
        let output: Vec<Value> = rows
            .iter()
            .map(|row| {
                let mut entry = json!({"id": row["id"]});
                if let Some(label) = row["label"].as_str().filter(|label| !label.is_empty()) {
                    entry["label"] = json!(label);
                }
                entry["scopes"] = scopes_of(row);
                entry["createdAt"] = json!(row["createdAt"]);
                entry["expiresAt"] = json!(row["expiresAt"]);
                entry
            })
            .collect();
        return format!("{}\n", serde_json::to_string_pretty(&output).expect("a value encodes"));
    }
    if rows.is_empty() {
        return "No active pairing credentials.\n".into();
    }
    let blocks: Vec<String> = rows
        .iter()
        .map(|row| {
            let label = row["label"].as_str().filter(|label| !label.is_empty()).map_or(String::new(), |label| format!(" ({label})"));
            let scopes: Vec<String> = scopes_of(row).as_array().into_iter().flatten().filter_map(|scope| scope.as_str().map(str::to_owned)).collect();
            [format!("{}{label}", row["id"].as_str().unwrap_or_default()), format!("  scopes: {}", scopes.join(" ")), format!("  created: {}", row["createdAt"].as_str().unwrap_or_default()), format!("  expires: {}", row["expiresAt"].as_str().unwrap_or_default())].join("\n")
        })
        .collect();
    format!("{}\n", blocks.join("\n\n"))
}

fn pairing_revoke(args: &[String], env: &dyn Fn(&str) -> Option<String>, now: DateTime<Utc>) -> Result<Outcome, String> {
    const SPEC: Spec = Spec { values: AUTH_LOCATION, booleans: &[], repeated: &[], positionals: (1, 1) };
    let parsed = parse(args, &SPEC)?;
    let id = &parsed.positionals[0];
    let (_, auth) = open_state(&state_dir(&parsed, env)?)?;
    let revoked = auth.revoke_pairing_link(id, now).map_err(|error| error.to_string())?;
    Ok(Outcome::log(if revoked { format!("Revoked pairing credential {id}.\n") } else { format!("No active pairing credential found for {id}.\n") }))
}

fn session_issue(args: &[String], env: &dyn Fn(&str) -> Option<String>, now: DateTime<Utc>) -> Result<Outcome, String> {
    const SPEC: Spec = Spec { values: &["base-dir", "state-dir", "scope", "ttl", "label", "subject"], booleans: &["token-only", "json"], repeated: &["scope"], positionals: (0, 0) };
    let parsed = parse(args, &SPEC)?;
    let scopes = scopes(&parsed, t3_contracts::AUTH_ADMINISTRATIVE_SCOPES)?;
    let ttl = ttl(&parsed, Duration::days(DEFAULT_SESSION_TTL_DAYS))?;
    let subject = parsed.value("subject").unwrap_or(DEFAULT_SESSION_SUBJECT);
    let mut client = json!({});
    if let Some(label) = parsed.value("label").filter(|label| !label.is_empty()) {
        client["label"] = json!(label);
    }
    client["deviceType"] = json!("bot");
    let (_, auth) = open_state(&state_dir(&parsed, env)?)?;
    let (session, token) = auth.issue_session(subject, "bearer-access-token", scopes, client, now, ttl).map_err(|error| error.to_string())?;
    let issued = json!({"sessionId": session.session_id, "token": token, "method": session.method, "scopes": scope_names(&session.scopes), "subject": session.subject, "client": session.client, "expiresAt": iso(session.expires_at)});
    Ok(Outcome::log(format_issued_session(&issued, parsed.flag("json"), parsed.flag("token-only"))))
}

/// `formatIssuedSession`.
pub fn format_issued_session(session: &Value, json_output: bool, token_only: bool) -> String {
    if token_only {
        return format!("{}\n", session["token"].as_str().unwrap_or_default());
    }
    if json_output {
        return format!("{}\n", serde_json::to_string_pretty(session).expect("a value encodes"));
    }
    let scopes: Vec<&str> = session["scopes"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    format!(
        "{}\n",
        [
            format!("Issued bearer access token {}.", session["sessionId"].as_str().unwrap_or_default()),
            format!("Scopes: {}", scopes.join(" ")),
            format!("Token: {}", session["token"].as_str().unwrap_or_default()),
            format!("Subject: {}", session["subject"].as_str().unwrap_or_default()),
            format!("Client: {}", format_client_metadata(&session["client"])),
            format!("Expires at: {}", session["expiresAt"].as_str().unwrap_or_default()),
        ]
        .join("\n")
    )
}

/// `formatClientMetadata`.
fn format_client_metadata(client: &Value) -> String {
    let text = |key: &str| client[key].as_str().filter(|value| !value.is_empty()).map(str::to_owned);
    let device = text("deviceType").filter(|device| device != "unknown");
    let details: Vec<String> = [text("label"), device, text("os"), text("browser"), text("ipAddress")].into_iter().flatten().collect();
    if details.is_empty() { "unlabeled client".into() } else { details.join(" | ") }
}

fn session_list(args: &[String], env: &dyn Fn(&str) -> Option<String>, now: DateTime<Utc>) -> Result<Outcome, String> {
    const SPEC: Spec = Spec { values: AUTH_LOCATION, booleans: &["json"], repeated: &[], positionals: (0, 0) };
    let parsed = parse(args, &SPEC)?;
    let (_, auth) = open_state(&state_dir(&parsed, env)?)?;
    let sessions: Vec<Value> = auth.list_client_sessions("", now).map_err(|error| error.to_string())?.iter().map(|session| serde_json::to_value(session).expect("a session encodes")).collect();
    Ok(Outcome::log(format_session_list(&sessions, parsed.flag("json"))))
}

/// `formatSessionList`, over the sessions as the wire carries them.
pub fn format_session_list(sessions: &[Value], json_output: bool) -> String {
    if json_output {
        let output: Vec<Value> = sessions
            .iter()
            .map(|session| {
                json!({"sessionId": session["sessionId"], "method": session["method"], "scopes": session["scopes"], "subject": session["subject"], "client": session["client"], "connected": session["connected"],
                    "issuedAt": session["issuedAt"], "expiresAt": session["expiresAt"], "lastConnectedAt": session.get("lastConnectedAt").cloned().unwrap_or(Value::Null)})
            })
            .collect();
        return format!("{}\n", serde_json::to_string_pretty(&output).expect("a value encodes"));
    }
    if sessions.is_empty() {
        return "No active sessions.\n".into();
    }
    let blocks: Vec<String> = sessions
        .iter()
        .map(|session| {
            let scopes: Vec<&str> = session["scopes"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            [
                format!("{}{}", session["sessionId"].as_str().unwrap_or_default(), if session["connected"] == true { " connected" } else { "" }),
                format!("  scopes: {}", scopes.join(" ")),
                format!("  method: {}", session["method"].as_str().unwrap_or_default()),
                format!("  subject: {}", session["subject"].as_str().unwrap_or_default()),
                format!("  client: {}", format_client_metadata(&session["client"])),
                format!("  issued: {}", session["issuedAt"].as_str().unwrap_or_default()),
                format!("  last connected: {}", session["lastConnectedAt"].as_str().unwrap_or("never")),
                format!("  expires: {}", session["expiresAt"].as_str().unwrap_or_default()),
            ]
            .join("\n")
        })
        .collect();
    format!("{}\n", blocks.join("\n\n"))
}

fn session_revoke(args: &[String], env: &dyn Fn(&str) -> Option<String>, now: DateTime<Utc>) -> Result<Outcome, String> {
    const SPEC: Spec = Spec { values: AUTH_LOCATION, booleans: &[], repeated: &[], positionals: (1, 1) };
    let parsed = parse(args, &SPEC)?;
    let id = &parsed.positionals[0];
    if id.trim().is_empty() {
        return Err("Invalid value for argument session-id: expected a non-empty string".into());
    }
    let (_, auth) = open_state(&state_dir(&parsed, env)?)?;
    let revoked = auth.revoke(id, now).map_err(|error| error.to_string())?;
    Ok(Outcome::log(if revoked { format!("Revoked session {id}.\n") } else { format!("No active session found for {id}.\n") }))
}

// ---------------------------------------------------------------------------------------------
// pair
// ---------------------------------------------------------------------------------------------

/// The scopes `pair` has always granted by default here: every one the RPC surface requires.
fn default_pair_scopes() -> Vec<AuthEnvironmentScope> {
    let mut scopes: Vec<AuthEnvironmentScope> = Vec::new();
    for (_, scope) in t3_contracts::RPC_REQUIRED_SCOPES.iter() {
        if scope.is_grantable() && !scopes.contains(scope) {
            scopes.push(*scope);
        }
    }
    scopes
}

fn pair_command(args: &[String], env: &dyn Fn(&str) -> Option<String>) -> Outcome {
    const SPEC: Spec = Spec { values: &["base-dir", "state-dir", "scope", "ttl", "label", "tailscale-serve-port"], booleans: &["tailscale"], repeated: &["scope"], positionals: (0, 0) };
    let run = || -> Result<Outcome, String> {
        let parsed = parse(args, &SPEC)?;
        let scopes = scopes(&parsed, &default_pair_scopes())?;
        let ttl = ttl(&parsed, Duration::minutes(DEFAULT_PAIRING_TTL_MINUTES))?;
        let state_dir = state_dir(&parsed, env)?;
        let (_, auth) = open_state(&state_dir)?;
        let now = Utc::now();
        if !parsed.flag("tailscale") {
            // What `pair` has printed here from the start: the credential alone, for a script to read.
            let issued = auth.issue_pairing_link(&scopes, "one-time-token", parsed.value("label"), now, ttl).map_err(|error| error.to_string())?;
            return Ok(Outcome::log(issued.credential));
        }
        let port = match parsed.value("tailscale-serve-port") {
            None => t3_remote::tailscale::DEFAULT_TAILSCALE_SERVE_PORT,
            Some(text) => text.parse::<u16>().ok().filter(|port| *port >= 1).ok_or_else(|| format!("Invalid value for flag --tailscale-serve-port: \"{text}\". Expected a port between 1 and 65535"))?,
        };
        let runtime = runtime()?;
        let tailscale = t3_remote::tailscale::Tailscale::new(cfg!(windows));
        let prober = t3_remote::tailscale_pair::HttpEnvironmentProber::default();
        let target = runtime.block_on(discover_target(&state_dir, &prober)).map_err(|error| error.to_string())?;
        let base = runtime.block_on(t3_remote::tailscale_pair::resolve_tailscale_pairing_base(&tailscale, &prober, &PairTarget { state: target.state.clone(), environment_id: target.environment_id.clone() }, port)).map_err(|error| error.to_string())?;
        let issued = auth.issue_pairing_link(&scopes, "one-time-token", Some(parsed.value("label").unwrap_or("t3 pair")), now, ttl).map_err(|error| error.to_string())?;
        let url = build_pairing_url(&base.base_url, &issued.credential)?;
        Ok(Outcome::log(format_pair_output(&target.label, &target.origin, &url, &issued.credential, issued.expires_at, &base.notes)))
    };
    run().unwrap_or_else(Outcome::failure)
}

/// The server a `pair` command found: the runtime state it recorded and what it said it is.
#[derive(Debug, Clone)]
pub struct DiscoveredTarget {
    pub state: PairServerState,
    pub environment_id: String,
    pub label: String,
    pub origin: String,
}

/// `discoverPairTarget`: the runtime state beside the database, a live process behind it, and an answer at its origin that is a T3
/// environment descriptor. The process check guards against a dead server's state file whose port was since reused by a
/// different server.
pub async fn discover_target(state_dir: &Path, prober: &dyn EnvironmentProber) -> Result<DiscoveredTarget, PairError> {
    let path = state_dir.join("server-runtime.json");
    let not_found = || PairError::NoRunningServer { checked_state_paths: vec![path.to_string_lossy().into_owned()] };
    let state: Value = std::fs::read_to_string(&path).ok().and_then(|text| serde_json::from_str(&text).ok()).ok_or_else(not_found)?;
    let pid = state["pid"].as_i64().filter(|pid| *pid > 0).ok_or_else(not_found)?;
    let port = state["port"].as_i64().and_then(|port| u16::try_from(port).ok()).ok_or_else(not_found)?;
    let origin = state["origin"].as_str().ok_or_else(not_found)?.to_owned();
    if !process_alive(pid) {
        return Err(not_found());
    }
    let EnvironmentProbeResult::Descriptor(descriptor) = prober.probe(&origin).await else { return Err(not_found()) };
    Ok(DiscoveredTarget {
        state: PairServerState { host: state["host"].as_str().map(str::to_owned), port, dev_url: state["devUrl"].as_str().map(str::to_owned) },
        environment_id: descriptor.environment_id.as_str().to_owned(),
        label: descriptor.label.as_str().to_owned(),
        origin,
    })
}

#[cfg(unix)]
fn process_alive(pid: i64) -> bool {
    let Ok(pid) = i32::try_from(pid) else { return false };
    // SAFETY: signal 0 only asks whether the process exists.
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}
#[cfg(not(unix))]
fn process_alive(_pid: i64) -> bool {
    true
}

/// `renderTerminalQrCode`: two QR rows to a line, in half blocks, with a margin of two modules.
pub fn render_terminal_qr_code(value: &str) -> String {
    use qrcodegen::{QrCode, QrCodeEcc};
    let margin = 2i32;
    let Ok(code) = QrCode::encode_text(value, QrCodeEcc::Medium) else { return String::new() };
    let size = code.size();
    let dark = |x: i32, y: i32| x >= 0 && x < size && y >= 0 && y < size && code.get_module(x, y);
    let mut rows = Vec::new();
    let mut y = -margin;
    while y < size + margin {
        let mut row = String::new();
        for x in -margin..size + margin {
            row.push(match (dark(x, y), dark(x, y + 1)) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        rows.push(row);
        y += 2;
    }
    rows.join("\n")
}

/// `formatPairOutput`.
pub fn format_pair_output(server_label: &str, origin: &str, pairing_url: &str, token: &str, expires_at: DateTime<Utc>, notes: &[String]) -> String {
    let mut lines = vec![
        format!("Pairing with {server_label} ({origin})."),
        String::new(),
        render_terminal_qr_code(pairing_url),
        String::new(),
        format!("Pairing URL: {pairing_url}"),
        format!("Token: {token}"),
        format!("Expires: {}", iso(expires_at)),
    ];
    for note in notes {
        lines.push(String::new());
        lines.push(format!("Note: {note}"));
    }
    lines.push(String::new());
    lines.join("\n")
}

/// The part of `pair --tailscale` that talks to Tailscale and the network, over the parts a test replaces.
pub async fn tailscale_pairing_base(tailscale: &dyn PairTailscale, prober: &dyn EnvironmentProber, state_dir: &Path, port: u16) -> Result<(DiscoveredTarget, t3_remote::tailscale_pair::TailscalePairingBase), PairError> {
    let target = discover_target(state_dir, prober).await?;
    let base = t3_remote::tailscale_pair::resolve_tailscale_pairing_base(tailscale, prober, &PairTarget { state: target.state.clone(), environment_id: target.environment_id.clone() }, port).await?;
    Ok((target, base))
}

// ---------------------------------------------------------------------------------------------
// project
// ---------------------------------------------------------------------------------------------

/// Where a project command runs: against the running server over HTTP, or straight on the state when none answers.
enum Mode {
    Live { origin: String, token: String },
    Offline,
}

fn project_command(args: &[String], env: &dyn Fn(&str) -> Option<String>) -> Outcome {
    let run = || -> Result<Outcome, String> {
        let (action, rest) = args.split_first().ok_or("Usage: project <add|remove|rename> ...")?;
        let runtime = runtime()?;
        match action.as_str() {
            "add" => {
                const SPEC: Spec = Spec { values: &["base-dir", "state-dir", "title"], booleans: &[], repeated: &[], positionals: (1, 1) };
                let parsed = parse(rest, &SPEC)?;
                runtime.block_on(project_add(&parsed, env))
            }
            "remove" => {
                const SPEC: Spec = Spec { values: &["base-dir", "state-dir"], booleans: &["force"], repeated: &[], positionals: (1, 1) };
                let parsed = parse(rest, &SPEC)?;
                runtime.block_on(project_remove(&parsed, env))
            }
            "rename" => {
                const SPEC: Spec = Spec { values: &["base-dir", "state-dir"], booleans: &[], repeated: &[], positionals: (2, 2) };
                let parsed = parse(rest, &SPEC)?;
                runtime.block_on(project_rename(&parsed, env))
            }
            other => Err(format!("Unknown command: project {other}")),
        }
    };
    run().unwrap_or_else(Outcome::failure)
}

/// The mode, the snapshot of active projects, and the way to dispatch.
struct Session {
    mode: Mode,
    store: Store,
    auth: AuthService,
    state_dir: PathBuf,
    http: reqwest::Client,
    session_id: Option<String>,
}

impl Session {
    /// `runProjectMutation`'s choice: a server recorded in `server-runtime.json` that answers the snapshot within a second is
    /// used; one that does not has its record cleared and the command runs on the state itself.
    async fn open(parsed: &Parsed, env: &dyn Fn(&str) -> Option<String>) -> Result<Self, String> {
        let state_dir = state_dir(parsed, env)?;
        let (store, auth) = open_state(&state_dir)?;
        let http = reqwest::Client::builder().no_proxy().build().map_err(|error| error.to_string())?;
        let mut session = Self { mode: Mode::Offline, store, auth, state_dir: state_dir.clone(), http, session_id: None };
        let runtime_path = state_dir.join("server-runtime.json");
        let origin = std::fs::read_to_string(&runtime_path).ok().and_then(|text| serde_json::from_str::<Value>(&text).ok()).and_then(|state| state["origin"].as_str().map(str::to_owned));
        if let Some(origin) = origin {
            let (issued, token) = session.auth.issue_session("cli-issued-session", "bearer-access-token", t3_contracts::AUTH_ADMINISTRATIVE_SCOPES.to_vec(), json!({"label":"t3 project cli","deviceType":"bot"}), Utc::now(), Duration::days(DEFAULT_SESSION_TTL_DAYS)).map_err(|error| error.to_string())?;
            session.session_id = Some(issued.session_id.clone());
            let reached = tokio::time::timeout(std::time::Duration::from_secs(1), session.http.get(format!("{origin}/api/projects")).bearer_auth(&token).send()).await;
            if reached.is_ok_and(|response| response.is_ok_and(|response| response.status().is_success())) {
                session.mode = Mode::Live { origin, token };
            } else {
                // TODO(original apps/server/src/cli/project.ts `tryResolveLiveProjectExecutionMode`): a server that is merely slow
                // to answer has its runtime record deleted as if it were gone. Copied.
                let _ = std::fs::remove_file(&runtime_path);
                session.finish();
            }
        }
        Ok(session)
    }
    /// Ends the temporary session the command made for itself.
    fn finish(&mut self) {
        if let Some(id) = self.session_id.take() {
            let _ = self.auth.revoke(&id, Utc::now());
        }
    }
    async fn snapshot(&self) -> Result<Vec<Value>, String> {
        match &self.mode {
            Mode::Offline => ProjectService::new(self.store.clone()).list().map_err(|error| error.to_string()),
            Mode::Live { origin, token } => {
                let response = tokio::time::timeout(std::time::Duration::from_secs(1), self.http.get(format!("{origin}/api/projects")).bearer_auth(token).send())
                    .await
                    .map_err(|_| "Failed to call the running server.".to_owned())?
                    .map_err(|_| "Failed to call the running server.".to_owned())?;
                live_failure(&response)?;
                let body: Value = response.json().await.map_err(|_| "Failed to call the running server.".to_owned())?;
                Ok(body["projects"].as_array().cloned().unwrap_or_default())
            }
        }
    }
    async fn dispatch(&self, command: Value) -> Result<(), String> {
        match &self.mode {
            Mode::Offline => ProjectService::new(self.store.clone()).mutate(command, Utc::now()).map(|_| ()).map_err(|error| crate::project::mutation_message(&error)),
            Mode::Live { origin, token } => {
                let response = tokio::time::timeout(std::time::Duration::from_secs(1), self.http.post(format!("{origin}/api/projects/mutate")).bearer_auth(token).json(&command).send())
                    .await
                    .map_err(|_| "Failed to call the running server.".to_owned())?
                    .map_err(|_| "Failed to call the running server.".to_owned())?;
                live_failure(&response)
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.finish();
    }
}

fn live_failure(response: &reqwest::Response) -> Result<(), String> {
    if response.status().is_success() {
        Ok(())
    } else {
        Err(format!("Server request failed with undeclared status {}.", response.status().as_u16()))
    }
}

fn is_active(project: &Value) -> bool {
    project["deletedAt"].is_null()
}

/// `normalizeWorkspaceRootForProjectCommand`.
fn normalize_root(root: &str) -> Result<String, String> {
    crate::project::normalize_workspace_root(root, false).map_err(|failure| failure["message"].as_str().or_else(|| failure["detail"].as_str()).map_or_else(|| failure.to_string(), str::to_owned))
}

/// `resolveProjectTitle`.
fn resolve_title(workspace_root: &str, explicit: Option<&str>) -> Result<String, String> {
    if let Some(explicit) = explicit {
        let trimmed = explicit.trim();
        return if trimmed.is_empty() { Err("Project title cannot be empty.".into()) } else { Ok(trimmed.to_owned()) };
    }
    let basename = Path::new(workspace_root).file_name().map(|name| name.to_string_lossy().trim().to_owned()).unwrap_or_default();
    Ok(if basename.is_empty() { "project".to_owned() } else { basename })
}

/// `findActiveProjectTarget`: by id, else by the workspace root the text names (the stored path still names a project after the
/// directory is gone).
fn find_target(snapshot: &[Value], identifier: &str) -> Result<Value, String> {
    let trimmed = identifier.trim();
    if trimmed.is_empty() {
        return Err("Project identifier cannot be empty.".into());
    }
    let active: Vec<&Value> = snapshot.iter().filter(|project| is_active(project)).collect();
    if let Some(project) = active.iter().find(|project| project["id"] == trimmed) {
        return Ok((*project).clone());
    }
    let normalized = normalize_root(trimmed).ok();
    let wanted = normalized.as_deref().unwrap_or(trimmed);
    active.iter().find(|project| project["workspaceRoot"] == wanted).map(|project| (*project).clone()).ok_or_else(|| format!("No active project found for '{trimmed}'."))
}

async fn project_add(parsed: &Parsed, env: &dyn Fn(&str) -> Option<String>) -> Result<Outcome, String> {
    let session = Session::open(parsed, env).await?;
    let root = normalize_root(&parsed.positionals[0])?;
    let snapshot = session.snapshot().await?;
    if snapshot.iter().any(|project| is_active(project) && project["workspaceRoot"] == root.as_str()) {
        return Err(format!("An active project already exists for '{root}'."));
    }
    let title = resolve_title(&root, parsed.value("title"))?;
    let id = uuid::Uuid::new_v4().to_string();
    session.dispatch(json!({"type":"project.create","commandId":uuid::Uuid::new_v4().to_string(),"projectId":id,"title":title,"workspaceRoot":root})).await?;
    Ok(Outcome::log(format!("Added project {id} ({title}) at {root}.")))
}

async fn project_remove(parsed: &Parsed, env: &dyn Fn(&str) -> Option<String>) -> Result<Outcome, String> {
    let session = Session::open(parsed, env).await?;
    let project = find_target(&session.snapshot().await?, &parsed.positionals[0])?;
    session.dispatch(json!({"type":"project.delete","commandId":uuid::Uuid::new_v4().to_string(),"projectId":project["id"],"force":parsed.flag("force")})).await?;
    Ok(Outcome::log(format!("Removed project {} ({}).", project["id"].as_str().unwrap_or_default(), project["title"].as_str().unwrap_or_default())))
}

async fn project_rename(parsed: &Parsed, env: &dyn Fn(&str) -> Option<String>) -> Result<Outcome, String> {
    let session = Session::open(parsed, env).await?;
    let project = find_target(&session.snapshot().await?, &parsed.positionals[0])?;
    let title = resolve_title(project["workspaceRoot"].as_str().unwrap_or_default(), Some(&parsed.positionals[1]))?;
    let id = project["id"].as_str().unwrap_or_default();
    if project["title"] == title.as_str() {
        return Ok(Outcome::log(format!("Project {id} is already named {title}.")));
    }
    session.dispatch(json!({"type":"project.update","commandId":uuid::Uuid::new_v4().to_string(),"projectId":id,"title":title})).await?;
    Ok(Outcome::log(format!("Renamed project {id} to {title}.")))
}
