//! Pi coding agent provider instance: executable resolution, version and health, authentication status,
//! models, slash commands and skills. Mirrors `PiProvider.ts` and `PiDriver.ts`.
//!
//! Health is `pi --version`. Models, the user's default thinking level and the user's commands (extension
//! slash commands, prompt templates, skills) come from a short-lived ephemeral RPC session
//! (`pi --mode rpc --no-session`), so everything the user configured in `~/.pi/agent` (custom providers,
//! `models.json` entries, extensions, skills) shows up without a hardcoded catalog. Pi has no sign-in
//! flow of its own for T3 Code to drive: a Pi with no usable models is reported unauthenticated and the
//! message says to run `pi` and use `/login`. It has no usage windows either.
use crate::{
    claude_models,
    pi_commands,
    pi_launch::{self, LaunchOptions},
    pi_protocol::{DEFAULT_REQUEST_TIMEOUT, Launch, PiProcess},
    provider_process::ProcessError,
};
use chrono::{SecondsFormat, Utc};
use serde_json::{Value, json};
use std::{collections::HashMap, path::{Path, PathBuf}, time::Duration};
use tokio::{io::AsyncReadExt, process::Command};

pub type PiConfig = t3_contracts::PiSettings;
const VERSION_TIMEOUT: Duration = Duration::from_secs(4);
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(15);
/// `MINIMUM_PI_VERSION`: `get_entries` arrived in 0.80.3 and `agent_settled` in source at 0.80.4; 0.80.5 was the first
/// published package with both. T3 needs them for rollback boundaries and reliable turn terminalization.
pub const MINIMUM_VERSION: &str = "0.80.5";
/// The slug that means "do not call `set_model`": Pi resolves the model from the user's own `settings.json`.
pub const INHERIT_MODEL_SLUG: &str = "default";

#[derive(Clone, Debug)]
pub struct PiInstance {
    pub instance_id: String,
    pub display_name: String,
    pub accent_color: Option<String>,
    pub enabled: bool,
    pub config: PiConfig,
    pub environment: HashMap<String, String>,
}
struct Output {
    code: Option<i32>,
    text: String,
}
/// What the ephemeral session reported.
struct Discovery {
    models: Vec<Value>,
    slash_commands: Vec<Value>,
    skills: Vec<Value>,
}

impl PiInstance {
    /// The configured binary; `pi` when the setting is empty.
    pub fn binary_path(&self) -> PathBuf {
        let configured = self.config.binary_path.as_str();
        PathBuf::from(if configured.is_empty() { "pi" } else { configured })
    }
    /// The environment every Pi process of this instance runs with, on top of the server's own.
    pub fn process_environment(&self) -> HashMap<String, String> {
        self.environment.clone()
    }
    fn custom_models(&self) -> Vec<Value> {
        serde_json::to_value(&self.config.custom_models).ok().and_then(|models| models.as_array().cloned()).unwrap_or_default()
    }
    /// `piModelsFromSettings`: the "Pi default" entry, what Pi listed, then the user's custom slugs.
    fn models(&self, discovered: Vec<Value>) -> Vec<Value> {
        let default = json!({"slug":INHERIT_MODEL_SLUG,"name":"Pi default","isCustom":false,"capabilities":{"optionDescriptors":[]}});
        claude_models::provider_models(std::iter::once(default).chain(discovered).collect(), &self.custom_models())
    }
    async fn run(&self, args: &[&str]) -> std::io::Result<Output> {
        let mut command = Command::new(self.binary_path());
        command.args(args).envs(self.process_environment()).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).kill_on_drop(true);
        let mut child = command.spawn()?;
        let (mut stdout, mut stderr) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
        let (mut out, mut err) = (String::new(), String::new());
        let _ = tokio::join!(stdout.read_to_string(&mut out), stderr.read_to_string(&mut err));
        let status = child.wait().await?;
        Ok(Output { code: status.code(), text: format!("{out}\n{err}") })
    }

    pub async fn discover(&self, cwd: &Path) -> Result<Value, ProcessError> {
        self.discover_scoped(cwd, true).await
    }
    /// Pi reports no usage windows, so a one-workspace scan differs from the machine's only in `cwd`.
    pub async fn discover_scoped(&self, cwd: &Path, _include_account: bool) -> Result<Value, ProcessError> {
        let checked_at = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        let mut snapshot = json!({
            "instanceId":self.instance_id,"driver":"pi","displayName":self.display_name,"enabled":self.enabled,
            "installed":false,"version":null,"status":"warning","auth":{"status":"unknown"},"checkedAt":checked_at,
            "availability":"available","models":self.models(vec![]),"slashCommands":[],"skills":[],
            "showInteractionModeToggle":false,"supportedRuntimeModes":["approval-required","auto-accept-edits","full-access"],
            // The adapter reports context usage from Pi's streaming usage while a turn runs, so clients can reserve the meter.
            "reportsContextWindow":true,"requiresNewThreadForModelChange":false,
            "continuation":{"groupKey":format!("pi:instance:{}", self.instance_id)}
        });
        if let Some(accent) = &self.accent_color {
            snapshot["accentColor"] = json!(accent);
        }
        if !self.enabled {
            snapshot["status"] = json!("disabled");
            snapshot["message"] = json!("Pi is disabled in T3 Code settings.");
            return validate(snapshot);
        }
        let fail = |mut snapshot: Value, installed: bool, version: Option<&str>, message: &str| {
            snapshot["installed"] = json!(installed);
            snapshot["version"] = json!(version);
            snapshot["status"] = json!("error");
            snapshot["message"] = json!(message);
            validate(snapshot)
        };
        let output = match tokio::time::timeout(VERSION_TIMEOUT, self.run(&["--version"])).await {
            Err(_) => return fail(snapshot, true, None, "Pi CLI is installed but timed out while running `pi --version`."),
            Ok(Err(error)) => {
                let missing = error.kind() == std::io::ErrorKind::NotFound;
                tracing::warn!(%error, "Pi CLI health check failed.");
                return fail(snapshot, !missing, None, if missing { "Pi CLI (`pi`) is not installed or not on PATH. Install with `npm install -g @earendil-works/pi-coding-agent`." } else { "Failed to execute Pi CLI health check." });
            }
            Ok(Ok(output)) => output,
        };
        let version = crate::claude_instance::parse_version(&output.text);
        if output.code != Some(0) {
            return fail(snapshot, true, version.as_deref(), "Pi CLI is installed but failed to run.");
        }
        let Some(version) = version else {
            return fail(snapshot, true, None, &format!("T3 Code could not determine the Pi version. Pi {MINIMUM_VERSION} or newer is required."));
        };
        if crate::provider_maintenance::compare_semver(&version, MINIMUM_VERSION) == std::cmp::Ordering::Less {
            return fail(snapshot, true, Some(&version), &format!("Pi {version} is unsupported. Update to Pi {MINIMUM_VERSION} or newer."));
        }
        let launch_args = match pi_launch::resolve_launch_args(self.config.launch_args.as_str()) {
            Ok(args) => args,
            Err(message) => return fail(snapshot, true, Some(&version), &message),
        };
        snapshot["installed"] = json!(true);
        snapshot["version"] = json!(version);
        snapshot["status"] = json!("ready");
        match tokio::time::timeout(DISCOVERY_TIMEOUT, self.discover_via_rpc(&launch_args, cwd)).await {
            Ok(Err(error)) => {
                tracing::warn!(%error, "Pi RPC discovery failed.");
                snapshot["message"] = json!("Pi is available, but T3 Code could not refresh its models and commands. The live session will retry startup.");
            }
            Err(_) => snapshot["message"] = json!("Pi is available, but model and command discovery needs interactive input. The live session will handle it."),
            Ok(Ok(discovery)) => {
                let authenticated = !discovery.models.is_empty();
                snapshot["models"] = json!(self.models(discovery.models));
                snapshot["slashCommands"] = json!(pi_commands::with_builtin_slash_commands(discovery.slash_commands));
                snapshot["skills"] = json!(discovery.skills);
                snapshot["status"] = json!(if authenticated { "ready" } else { "warning" });
                snapshot["auth"] = json!({"status":if authenticated { "authenticated" } else { "unauthenticated" },"type":"pi"});
                if !authenticated {
                    snapshot["message"] = json!("Pi has no usable models. Run `pi` in a terminal and use /login, or configure an API key in ~/.pi/agent.");
                }
            }
        }
        validate(snapshot)
    }

    /// `discoverPiViaRpc`.
    async fn discover_via_rpc(&self, launch_args: &[String], cwd: &Path) -> Result<Discovery, String> {
        let launch = pi_launch::build_rpc_launch(&LaunchOptions { launch_args, mcp: None, extension_path: None, ephemeral: true, runtime_mode: None, disable_extensions: false, disable_tools: false });
        let mut env = self.process_environment();
        env.extend(launch.env);
        let (process, _events) = PiProcess::spawn(&Launch { binary: self.binary_path(), args: launch.args, cwd: cwd.to_path_buf(), env, remove_env: launch.remove_env })?;
        let discovered = async {
            let state = process.request(json!({"type":"get_state"}), DEFAULT_REQUEST_TIMEOUT).await.map_err(|error| error.to_string())?;
            let models = process.request(json!({"type":"get_available_models"}), DEFAULT_REQUEST_TIMEOUT).await.map_err(|error| error.to_string())?;
            let commands = process.request(json!({"type":"get_commands"}), DEFAULT_REQUEST_TIMEOUT).await.unwrap_or(Value::Null);
            let (slash_commands, skills) = pi_commands::parse_discovered_commands(&commands);
            Ok::<_, String>(Discovery { models: parse_discovered_models(&models, &state["thinkingLevel"]), slash_commands, skills })
        }
        .await;
        process.terminate().await;
        discovered
    }
}

/// `parseDiscoveredModels`: `provider/id` slugs, each Pi model once, with the thinking levels it supports.
fn parse_discovered_models(data: &Value, default_thinking: &Value) -> Vec<Value> {
    let mut seen = std::collections::HashSet::new();
    let mut parsed = vec![];
    for model in data["models"].as_array().into_iter().flatten() {
        let field = |key: &str| model.get(key).and_then(Value::as_str).map(str::trim).filter(|text| !text.is_empty());
        let (Some(provider), Some(id)) = (field("provider"), field("id")) else { continue };
        let slug = format!("{provider}/{id}");
        if !seen.insert(slug.clone()) {
            continue;
        }
        parsed.push(json!({"slug":slug,"name":field("name").unwrap_or(&slug),"subProvider":provider,"isCustom":false,"capabilities":pi_commands::thinking_capabilities(model, default_thinking)}));
    }
    parsed
}
fn validate(snapshot: Value) -> Result<Value, ProcessError> {
    let typed: t3_contracts::ServerProvider = serde_json::from_value(snapshot).map_err(|error| ProcessError::Protocol(error.to_string()))?;
    serde_json::to_value(typed).map_err(|error| ProcessError::Protocol(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovered_models_are_unique_provider_slugs_with_their_thinking_ladder() {
        let data = json!({"models":[
            {"provider":"openrouter","id":"deepseek/v4","name":"DeepSeek V4","reasoning":true,"thinkingLevelMap":{"off":"none","minimal":null,"low":null}},
            {"provider":"openrouter","id":"deepseek/v4","name":"Duplicate"},
            {"provider":" local ","id":" tiny "},
            {"provider":"nope"},{"id":"nope"},"junk"
        ]});
        let models = parse_discovered_models(&data, &json!("high"));
        assert_eq!(models.len(), 2);
        assert_eq!((models[0]["slug"].as_str(), models[0]["name"].as_str(), models[0]["subProvider"].as_str()), (Some("openrouter/deepseek/v4"), Some("DeepSeek V4"), Some("openrouter")));
        let ladder: Vec<&str> = models[0]["capabilities"]["optionDescriptors"][0]["options"].as_array().unwrap().iter().map(|option| option["id"].as_str().unwrap()).collect();
        assert_eq!(ladder, ["off", "medium", "high"]);
        assert_eq!(models[1], json!({"slug":"local/tiny","name":"local/tiny","subProvider":"local","isCustom":false,"capabilities":{"optionDescriptors":[]}}));
    }
}
