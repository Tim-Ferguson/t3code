//! Claude Code provider instance: executable resolution, health and authentication
//! status, model list. Mirrors `checkClaudeProviderStatus` in `ClaudeProvider.ts`.
use crate::{claude_launch, claude_models, claude_protocol, claude_reset_credits, claude_skills, claude_usage_limits, provider_process::ProcessError};
use chrono::{SecondsFormat, Utc};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command};

pub type ClaudeConfig = t3_contracts::ClaudeSettings;
/// Matches `DEFAULT_TIMEOUT_MS` for `claude --version`.
const VERSION_TIMEOUT: Duration = Duration::from_secs(4);
/// Matches `CAPABILITIES_PROBE_TIMEOUT_MS`; Bedrock initializes slowly.
const PROBE_TIMEOUT: Duration = Duration::from_secs(25);

#[derive(Clone, Debug)]
pub struct ClaudeInstance {
    pub instance_id: String,
    pub display_name: String,
    pub accent_color: Option<String>,
    pub enabled: bool,
    pub config: ClaudeConfig,
    pub environment: HashMap<String, String>,
}

/// What the initialization handshake reports without a model request.
#[derive(Debug, Default, PartialEq)]
pub struct Capabilities {
    pub email: Option<String>,
    pub subscription_type: Option<String>,
    pub token_source: Option<String>,
    pub api_provider: Option<String>,
    pub slash_commands: Vec<Value>,
    /// The `get_usage` reply, `None` when that request failed. Absent windows on a successful reply
    /// mean the account has none (an API key).
    pub usage: Option<Value>,
}

impl ClaudeInstance {
    /// The configured binary with a leading `~` expanded.
    pub fn binary_path(&self) -> PathBuf {
        let expanded = expand_home(self.config.binary_path.as_str(), &self.environment);
        // On Windows a bare name or an npm launcher shim cannot be spawned directly.
        let platform = crate::provider_maintenance::Platform::host();
        if platform != crate::provider_maintenance::Platform::Win32 {
            return expanded;
        }
        let mut environment: HashMap<String, String> = std::env::vars().collect();
        environment.extend(self.process_environment());
        let resolved = claude_launch::resolve_sdk_executable(&expanded.to_string_lossy(), platform, &|command| crate::provider_maintenance::resolve_command_path(command, &environment, platform), &|path| Path::new(path).is_file());
        PathBuf::from(resolved)
    }
    /// The instance environment plus `CLAUDE_CONFIG_DIR` when the instance has its own home
    /// (the CLI keeps `HOME`, and with it the macOS keychain, untouched).
    pub fn process_environment(&self) -> HashMap<String, String> {
        let mut environment = self.environment.clone();
        if let Some(home) = self.config_dir_override() {
            environment.insert("CLAUDE_CONFIG_DIR".into(), home.to_string_lossy().into_owned());
        }
        environment
    }
    fn config_dir_override(&self) -> Option<PathBuf> {
        let home = self.config.home_path.as_str().trim();
        (!home.is_empty()).then(|| absolute(&expand_home(home, &self.environment)))
    }
    /// `resolveClaudeHomePath`: the instance home, an inherited `CLAUDE_CONFIG_DIR`, then `~/.claude`.
    pub fn config_dir(&self) -> PathBuf {
        if let Some(home) = self.config_dir_override() {
            return home;
        }
        let inherited = self.environment.get("CLAUDE_CONFIG_DIR").cloned().or_else(|| std::env::var("CLAUDE_CONFIG_DIR").ok());
        match inherited.as_deref().map(str::trim).filter(|value| !value.is_empty()) {
            Some(dir) => absolute(Path::new(dir)),
            None => std::env::var_os("HOME").map(|home| Path::new(&home).join(".claude")).unwrap_or_else(|| PathBuf::from(".claude")),
        }
    }

    /// The skills Claude Code would load for `cwd`: the config directory's and the workspace's.
    pub fn discover_skills(&self, cwd: Option<&Path>) -> Vec<Value> {
        let lookup = |name: &str| self.environment.get(name).cloned().or_else(|| std::env::var(name).ok());
        let environment: HashMap<String, String> = ["CLAUDE_CONFIG_DIR", "PROGRAMDATA"].into_iter().filter_map(|name| lookup(name).map(|value| (name.to_owned(), value))).collect();
        let home = lookup("HOME").map(PathBuf::from).unwrap_or_default();
        let platform = if cfg!(target_os = "macos") { "darwin" } else if cfg!(windows) { "win32" } else { "linux" };
        let roots = claude_skills::Roots { home_path: self.config.home_path.as_str(), environment: &environment, home: &home, managed_settings: claude_skills::managed_settings_path(platform, &environment) };
        claude_skills::discover(&roots, cwd)
    }

    pub async fn discover(&self, cwd: &Path) -> Result<Value, ProcessError> {
        self.discover_scoped(cwd, true).await
    }
    /// `include_usage` is false for a one-workspace scan: usage windows belong to the machine's snapshot.
    pub async fn discover_scoped(&self, cwd: &Path, include_usage: bool) -> Result<Value, ProcessError> {
        let checked_at = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        let custom = serde_json::to_value(&self.config.custom_models).map_err(|error| ProcessError::Protocol(error.to_string()))?;
        let custom = custom.as_array().cloned().unwrap_or_default();
        let all_models = claude_models::provider_models(claude_models::all_models(), &custom);
        let mut snapshot = json!({
            "instanceId":self.instance_id,"driver":"claudeAgent","displayName":self.display_name,"enabled":self.enabled,
            "installed":false,"version":null,"status":"warning","auth":{"status":"unknown"},"checkedAt":checked_at,
            "availability":"available","models":all_models,"slashCommands":[],"skills":[],
            "showInteractionModeToggle":true,"reportsContextWindow":true,
            "continuation":{"groupKey":format!("claude:home:{}", self.config_dir().to_string_lossy())}
        });
        if let Some(accent) = &self.accent_color {
            snapshot["accentColor"] = json!(accent);
        }
        if !self.enabled {
            snapshot["status"] = json!("disabled");
            snapshot["message"] = json!("Claude is disabled in T3 Code settings.");
            return validate(snapshot);
        }
        let output = match tokio::time::timeout(VERSION_TIMEOUT, self.run_version()).await {
            Err(_) => {
                snapshot["installed"] = json!(true);
                snapshot["status"] = json!("error");
                snapshot["message"] = json!("Claude Agent CLI is installed but failed to run. Timed out while running command.");
                return validate(snapshot);
            }
            Ok(Err(error)) => {
                let missing = error.kind() == std::io::ErrorKind::NotFound;
                tracing::warn!(%error, "Claude Agent CLI health check failed.");
                snapshot["installed"] = json!(!missing);
                snapshot["status"] = json!("error");
                snapshot["message"] = json!(if missing { "Claude Agent CLI (`claude`) was not found on PATH." } else { "Failed to execute Claude Agent CLI health check." });
                return validate(snapshot);
            }
            Ok(Ok(output)) => output,
        };
        let version = parse_version(&output.text);
        snapshot["installed"] = json!(true);
        snapshot["version"] = json!(version);
        if !output.success {
            snapshot["status"] = json!("error");
            snapshot["message"] = json!("Claude Agent CLI is installed but failed to run.");
            return validate(snapshot);
        }
        snapshot["models"] = json!(claude_models::provider_models(claude_models::models_for_version(version.as_deref()), &custom));
        snapshot["skills"] = json!(self.discover_skills(Some(cwd)));
        let upgrade = claude_models::upgrade_message(version.as_deref());
        let capabilities = tokio::time::timeout(PROBE_TIMEOUT, self.probe_with(cwd, include_usage)).await.ok().flatten();
        let compact = json!({"name":"compact","description":"Summarize the conversation and reduce context usage"});
        let mut commands = vec![compact];
        let Some(capabilities) = capabilities else {
            snapshot["slashCommands"] = json!(dedupe_commands(commands));
            snapshot["message"] = json!("Could not verify Claude authentication status from initialization result.");
            return validate(snapshot);
        };
        commands.extend(capabilities.slash_commands.iter().cloned());
        snapshot["slashCommands"] = json!(dedupe_commands(commands));
        snapshot["status"] = json!("ready");
        let usage_limits = match &capabilities.usage {
            None if !include_usage => Value::Null,
            None => claude_usage_limits::make_unavailable_usage_limits(&checked_at, "probeFailed", None),
            Some(usage) => {
                let (limits, names) = claude_usage_limits::usage_response_to_limits(usage, &checked_at);
                claude_usage_limits::record_scoped_names(&self.instance_id, names);
                limits
            }
        };
        let mut usage_limits = usage_limits;
        if let (Some(_), true, Some(version)) = (&capabilities.subscription_type, !usage_limits.is_null() && usage_limits["unavailable"].is_null(), &version) {
            if let Some(credits) = claude_reset_credits::read_credits(&self.config_dir(), version, &self.process_environment()).await {
                usage_limits["resetCredits"] = credits;
            }
        }
        if !usage_limits.is_null() {
            snapshot["usageLimits"] = usage_limits;
        }
        let mut auth = json!({"status":"authenticated"});
        if let Some(email) = &capabilities.email {
            auth["email"] = json!(email);
        }
        if let Some((kind, label)) = auth_metadata(capabilities.subscription_type.as_deref(), capabilities.token_source.as_deref()).or_else(|| (capabilities.api_provider.as_deref() == Some("bedrock")).then(|| ("bedrock".into(), "Amazon Bedrock".into()))) {
            auth["type"] = json!(kind);
            auth["label"] = json!(label);
        }
        snapshot["auth"] = auth;
        if let Some(upgrade) = upgrade {
            snapshot["message"] = json!(upgrade);
        }
        validate(snapshot)
    }
    async fn run_version(&self) -> std::io::Result<VersionOutput> {
        let mut command = Command::new(self.binary_path());
        command.arg("--version").envs(self.process_environment()).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).kill_on_drop(true);
        let mut child = command.spawn()?;
        let mut text = String::new();
        if let Some(mut stdout) = child.stdout.take() {
            let _ = stdout.read_to_string(&mut text).await;
        }
        if let Some(mut stderr) = child.stderr.take() {
            let _ = stderr.read_to_string(&mut text).await;
        }
        let status = child.wait().await?;
        Ok(VersionOutput { success: status.success(), text })
    }
    /// Spawns the CLI as the SDK does for its health probe and reads the `initialize` response.
    /// No user message is written, so no model request is made.
    pub async fn probe_capabilities(&self, cwd: &Path) -> Option<Capabilities> {
        self.probe_with(cwd, true).await
    }
    async fn probe_with(&self, cwd: &Path, include_usage: bool) -> Option<Capabilities> {
        let options = claude_launch::probe_options(&self.binary_path().to_string_lossy(), cwd, self.process_environment());
        let (process, mut events) = claude_protocol::ClaudeProcess::spawn(&options.launch()).ok()?;
        let reply = tokio::select! {
            reply = process.request(options.initialize_request(), PROBE_TIMEOUT) => reply.ok(),
            _ = async { while let Some(event) = events.recv().await { if matches!(event, claude_protocol::Event::Closed(_)) { break; } } } => None,
        };
        let response = match reply {
            Some(response) => response,
            None => {
                process.shutdown().await;
                return None;
            }
        };
        // Usage has its own deadline so a slow optional request cannot discard initialization.
        // The local transcript scan that fills `behaviors` is skipped: it outlasts the deadline.
        let usage = if include_usage { process.request(json!({"subtype":"get_usage","skip_behaviors":true}), VERSION_TIMEOUT).await.ok().filter(Value::is_object) } else { None };
        process.shutdown().await;
        let account = &response["account"];
        let text = |value: &Value| value.as_str().map(str::trim).filter(|value| !value.is_empty()).map(str::to_owned);
        let slash_commands = response["commands"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|command| {
                let name = text(&command["name"])?;
                let mut entry = json!({"name":name});
                if let Some(description) = text(&command["description"]) {
                    entry["description"] = json!(description);
                }
                if let Some(hint) = text(&command["argumentHint"]) {
                    entry["input"] = json!({"hint":hint});
                }
                Some(entry)
            })
            .collect();
        Some(Capabilities {
            email: text(&account["email"]),
            subscription_type: text(&account["subscriptionType"]),
            token_source: text(&account["tokenSource"]),
            api_provider: text(&account["apiProvider"]),
            slash_commands,
            usage,
        })
    }
}
struct VersionOutput {
    success: bool,
    text: String,
}
fn validate(snapshot: Value) -> Result<Value, ProcessError> {
    let typed: t3_contracts::ServerProvider = serde_json::from_value(snapshot).map_err(|error| ProcessError::Protocol(error.to_string()))?;
    serde_json::to_value(typed).map_err(|error| ProcessError::Protocol(error.to_string()))
}
/// `parseGenericCliVersion`: the first `x.y.z`, with an optional `v` prefix.
pub fn parse_version(output: &str) -> Option<String> {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| regex::Regex::new(r"\bv?(\d+\.\d+\.\d+)\b").unwrap()).captures(output).map(|captures| captures[1].to_owned())
}
fn dedupe_commands(commands: Vec<Value>) -> Vec<Value> {
    let mut merged: Vec<Value> = vec![];
    for command in commands {
        let Some(name) = command["name"].as_str().map(str::trim).filter(|name| !name.is_empty()).map(str::to_owned) else { continue };
        match merged.iter_mut().find(|existing| existing["name"].as_str().is_some_and(|existing| existing.to_lowercase() == name.to_lowercase())) {
            None => {
                let mut command = command.clone();
                command["name"] = json!(name);
                merged.push(command);
            }
            Some(existing) => {
                if existing["description"].is_null() && !command["description"].is_null() {
                    existing["description"] = command["description"].clone();
                }
                if existing["input"]["hint"].is_null() && !command["input"]["hint"].is_null() {
                    existing["input"] = json!({"hint":command["input"]["hint"]});
                }
            }
        }
    }
    merged
}
fn title_case_words(value: &str) -> String {
    value
        .split(|c: char| c.is_whitespace() || c == '_' || c == '-')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            chars.next().map(|first| first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}
fn squash(value: &str) -> String {
    value.to_lowercase().chars().filter(|c| !(c.is_whitespace() || *c == '_' || *c == '-')).collect()
}
fn subscription_label(subscription: &str) -> String {
    match squash(subscription).as_str() {
        "claudemaxsubscription" | "max" | "maxplan" => "Max".into(),
        "claudemax5xsubscription" | "max5" => "Max 5x".into(),
        "claudemax20xsubscription" | "max20" => "Max 20x".into(),
        "claudeenterprisesubscription" | "enterprise" => "Enterprise".into(),
        "claudeteamsubscription" | "team" => "Team".into(),
        "claudeprosubscription" | "pro" => "Pro".into(),
        "claudefreesubscription" | "free" => "Free".into(),
        _ => title_case_words(subscription),
    }
}
fn subscription_auth_label(subscription: &str) -> String {
    let label = subscription_label(subscription);
    let normalized = squash(&label);
    if normalized.starts_with("claude") && normalized.ends_with("subscription") {
        label
    } else if normalized.starts_with("claude") {
        format!("{label} Subscription")
    } else if normalized.ends_with("subscription") {
        format!("Claude {label}")
    } else {
        format!("Claude {label} Subscription")
    }
}
/// `claudeAuthMetadata`: an API key login, or the subscription plan.
fn auth_metadata(subscription: Option<&str>, token_source: Option<&str>) -> Option<(String, String)> {
    if token_source.is_some_and(|method| matches!(squash(method).as_str(), "apikey" | "anthropicapikey" | "anthropicauthtoken")) {
        return Some(("apiKey".into(), "Claude API Key".into()));
    }
    subscription.map(|subscription| (subscription.to_owned(), subscription_auth_label(subscription)))
}
fn expand_home(value: &str, environment: &HashMap<String, String>) -> PathBuf {
    let home = environment.get("HOME").cloned().or_else(|| std::env::var("HOME").ok());
    match (value.strip_prefix("~/").or((value == "~").then_some("")), home) {
        (Some(rest), Some(home)) => Path::new(&home).join(rest),
        _ => PathBuf::from(value),
    }
}
fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn auth_labels_follow_the_source_normalization() {
        assert_eq!(auth_metadata(None, Some("ANTHROPIC_API_KEY")), Some(("apiKey".into(), "Claude API Key".into())));
        assert_eq!(auth_metadata(Some("max"), None), Some(("max".into(), "Claude Max Subscription".into())));
        assert_eq!(auth_metadata(Some("claude_max_20x_subscription"), None).unwrap().1, "Claude Max 20x Subscription");
        assert_eq!(auth_metadata(Some("some_new_plan"), None).unwrap().1, "Claude Some New Plan Subscription");
        assert_eq!(auth_metadata(None, None), None);
        assert_eq!(parse_version("2.1.111 (Claude Code)\n").as_deref(), Some("2.1.111"));
        assert_eq!(parse_version("claude v3.0.1-beta").as_deref(), Some("3.0.1"));
        assert_eq!(parse_version("no version"), None);
    }
    #[test]
    fn commands_dedupe_case_insensitively_keeping_the_first_description() {
        let merged = dedupe_commands(vec![json!({"name":"compact","description":"Summarize"}), json!({"name":"Compact","description":"Other"}), json!({"name":"init"}), json!({"name":" "})]);
        assert_eq!(merged, vec![json!({"name":"compact","description":"Summarize"}), json!({"name":"init"})]);
    }
}
