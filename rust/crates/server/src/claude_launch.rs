//! What the T3 Claude adapter asks the CLI for: runtime policy to permission mode, model
//! selection to flags and settings, MCP attachment and the system prompt addendum.
//! Mirrors `makeClaudeQueryOptions`, `claudeRuntimeQueryPolicyForRuntimePolicy` and
//! `claudeMcpQueryOverrides` in `ClaudeAdapterV2.ts`.
use crate::claude_protocol::{QueryOptions, SessionIdentity, Thinking, Tools};
use serde_json::{Value, json};
use std::{collections::HashMap, path::Path};

pub const T3_MCP_TOOL_WILDCARD: &str = "mcp__t3-code__*";
/// Claude Code aborts an HTTP MCP call after 60 s unless the server sets `timeout`; T3's wait
/// tools legitimately block for up to an hour.
const T3_MCP_TOOL_TIMEOUT_MS: u64 = 65 * 60 * 1000;

/// The permission mode a thread's runtime and interaction mode select.
#[derive(Debug, Clone, PartialEq)]
pub struct Policy {
    pub permission_mode: &'static str,
    /// Whether tool permission prompts must be put to the user (the callback asks rather than allows).
    pub ask_user: bool,
    pub allow_dangerously_skip_permissions: bool,
}
pub fn policy(runtime_mode: &str, interaction_mode: &str) -> Policy {
    let ask_user = matches!(runtime_mode, "approval-required" | "auto-accept-edits");
    let permission_mode = if interaction_mode == "plan" {
        "plan"
    } else {
        match runtime_mode {
            "auto-accept-edits" => "acceptEdits",
            "auto" => "auto",
            "full-access" => "bypassPermissions",
            _ => "default",
        }
    };
    Policy { permission_mode, ask_user, allow_dangerously_skip_permissions: permission_mode == "bypassPermissions" }
}

/// Thread-scoped T3 MCP credentials as the CLI is given them.
pub struct Mcp<'a> {
    pub endpoint: &'a str,
    pub authorization_header: &'a str,
}
pub struct LaunchInput<'a> {
    pub binary_path: &'a str,
    pub launch_args: &'a str,
    pub auto_compact_window: &'a str,
    pub environment: HashMap<String, String>,
    pub selection: &'a Value,
    pub native_thread_id: &'a str,
    pub resume: bool,
    pub resume_session_at: Option<&'a str>,
    /// `--fork-session`: the resumed conversation continues as a new session (a fork's first turn).
    pub fork_session: bool,
    pub cwd: &'a Path,
    pub attachments_dir: Option<&'a Path>,
    pub runtime_mode: &'a str,
    pub interaction_mode: &'a str,
    pub mcp: Option<Mcp<'a>>,
}

/// `resolveClaudeSdkExecutablePath`: a value the CLI can be spawned from directly. The SDK (and
/// this port) spawns the path without a shell or Windows PATH/PATHEXT resolution, so a bare `claude`
/// or an npm `claude.cmd` shim does not work there. On Windows the command is resolved against
/// PATH and an npm launcher shim is followed to the package entry (`bin/claude.exe`, or `cli.js`
/// for older package versions). Elsewhere the configured value is unchanged.
pub fn resolve_sdk_executable(binary_path: &str, platform: crate::provider_maintenance::Platform, resolve: &dyn Fn(&str) -> Option<String>, is_file: &dyn Fn(&str) -> bool) -> String {
    if platform != crate::provider_maintenance::Platform::Win32 {
        return binary_path.to_owned();
    }
    let resolved = resolve(binary_path).unwrap_or_else(|| binary_path.to_owned());
    let extension = resolved.rsplit(['\\', '/']).next().and_then(|name| name.rsplit_once('.')).map(|(_, extension)| extension.to_lowercase());
    if !matches!(extension.as_deref(), Some("cmd" | "bat" | "ps1")) {
        return resolved;
    }
    let directory = resolved.rsplit_once(['\\', '/']).map_or("", |(directory, _)| directory);
    for entry in [r"node_modules\@anthropic-ai\claude-code\bin\claude.exe", r"node_modules\@anthropic-ai\claude-code\cli.js"] {
        let candidate = format!("{directory}\\{entry}");
        if is_file(&candidate) {
            return candidate;
        }
    }
    tracing::warn!(%binary_path, resolved_shim_path = %resolved, "Claude launcher shim resolved but no known package entry was found next to it; scripts cannot be spawned directly.");
    binary_path.to_owned()
}

/// `--launch-args` flags as `parseCliArgs` reads them (`--key value`, `--key=value`, `--flag`).
fn launch_flags(launch_args: &str) -> Vec<(String, Option<String>)> {
    let tokens = crate::codex::tokenize_cli_args(launch_args);
    let mut flags: Vec<(String, Option<String>)> = vec![];
    let mut index = 0;
    while index < tokens.len() {
        let token = &tokens[index];
        index += 1;
        let Some(rest) = token.strip_prefix("--").filter(|rest| !rest.is_empty()) else { continue };
        let (key, value) = match rest.split_once('=') {
            Some((key, value)) => (key.to_owned(), Some(value.to_owned())),
            None => match tokens.get(index).filter(|next| !next.starts_with("--")) {
                Some(next) => {
                    index += 1;
                    (rest.to_owned(), Some(next.clone()))
                }
                None => (rest.to_owned(), None),
            },
        };
        match flags.iter_mut().find(|(existing, _)| *existing == key) {
            Some(existing) => existing.1 = value,
            None => flags.push((key, value)),
        }
    }
    flags
}

pub fn query_options(input: LaunchInput) -> QueryOptions {
    let compiled = crate::claude_models::compile_selection(input.selection);
    let policy = policy(input.runtime_mode, input.interaction_mode);
    let mut extra: Vec<(String, Option<String>)> = launch_flags(input.launch_args);
    let take = |extra: &mut Vec<(String, Option<String>)>, key: &str| extra.iter().position(|(name, _)| name == key).map(|at| extra.remove(at));
    let permission_flag = take(&mut extra, "permission-mode");
    let skip_flag = take(&mut extra, "dangerously-skip-permissions");
    let thinking_display = extra.iter().find(|(key, _)| key == "thinking-display").map(|(_, value)| value.clone());
    let request_summaries = compiled.settings.get("alwaysThinkingEnabled") != Some(&json!(false)) && thinking_display != Some(Some("omitted".into()));
    if request_summaries && thinking_display.is_none() {
        extra.push(("thinking-display".into(), Some("summarized".into())));
    }
    let permission_mode = match (permission_flag, skip_flag) {
        (Some((_, Some(mode))), _) => mode,
        (_, Some((_, None))) => "bypassPermissions".into(),
        (_, Some((_, Some(value)))) if value == "true" => "bypassPermissions".into(),
        _ => policy.permission_mode.to_owned(),
    };
    let mut settings = compiled.settings.clone();
    if let Ok(window) = input.auto_compact_window.trim().parse::<f64>().map(|value| if value.fract() == 0.0 { json!(value as i64) } else { json!(value) }) {
        settings.insert("autoCompactWindow".into(), window);
    }
    if request_summaries {
        settings.insert("showThinkingSummaries".into(), json!(true));
    }
    let mut allowed_tools = vec![];
    let mut mcp_servers = None;
    let mut append = crate::provider_instructions::runtime("Claude Code", "", None, None);
    if let Some(mcp) = &input.mcp {
        allowed_tools.push(T3_MCP_TOOL_WILDCARD.to_owned());
        mcp_servers = Some(json!({"t3-code":{"type":"http","url":mcp.endpoint,"headers":{"Authorization":mcp.authorization_header},"timeout":T3_MCP_TOOL_TIMEOUT_MS}}));
        append.push_str(orchestration_instructions());
    }
    let mut additional_directories = vec![input.cwd.to_string_lossy().into_owned()];
    additional_directories.extend(input.attachments_dir.map(|directory| directory.to_string_lossy().into_owned()));
    QueryOptions {
        binary: input.binary_path.into(),
        cwd: input.cwd.into(),
        env: input.environment,
        model: compiled.api_model_id,
        effort: compiled.effort,
        thinking: request_summaries.then(|| Thinking::Adaptive { display: Some("summarized".into()) }),
        tools: Some(Tools::Preset),
        allow_dangerously_skip_permissions: policy.allow_dangerously_skip_permissions && policy.permission_mode != "plan",
        permission_mode,
        include_partial_messages: true,
        identity: Some(if input.resume { SessionIdentity::Resume(input.native_thread_id.into()) } else { SessionIdentity::New(input.native_thread_id.into()) }),
        resume_session_at: input.resume_session_at.map(str::to_owned),
        fork_session: input.fork_session,
        allowed_tools,
        disallowed_tools: vec![],
        can_use_tool: true,
        mcp_servers,
        strict_mcp_config: false,
        setting_sources: None,
        persist_session: true,
        additional_directories,
        settings: (!settings.is_empty()).then(|| Value::Object(settings)),
        extra_args: extra,
        append_system_prompt: Some(append),
        system_prompt: None,
        supported_dialog_kinds: vec!["resume_return".into()],
    }
}
/// The T3 Code orchestration addendum, appended when the T3 MCP server is attached.
fn orchestration_instructions() -> &'static str {
    static TEXT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    TEXT.get_or_init(|| crate::provider_instructions::codex_context("", "", false, false)["t3_code_orchestration"]["value"].as_str().unwrap().to_owned())
}

/// Options of the periodic health probe (`buildClaudeCapabilitiesProbeQueryOptions`): no session
/// file, user/project/local settings only, hooks and MCP servers disabled, no model request.
pub fn probe_options(binary_path: &str, cwd: &Path, mut environment: HashMap<String, String>) -> QueryOptions {
    environment.insert("ENABLE_CLAUDEAI_MCP_SERVERS".into(), "false".into());
    environment.insert("CLAUDE_CODE_AUTO_CONNECT_IDE".into(), "0".into());
    environment.insert("CLAUDE_CODE_IDE_SKIP_AUTO_INSTALL".into(), "1".into());
    QueryOptions {
        binary: binary_path.into(),
        cwd: cwd.into(),
        env: environment,
        model: String::new(),
        effort: None,
        thinking: None,
        tools: None,
        permission_mode: "default".into(),
        allow_dangerously_skip_permissions: false,
        include_partial_messages: false,
        identity: None,
        resume_session_at: None,
        fork_session: false,
        allowed_tools: vec![],
        disallowed_tools: vec![],
        can_use_tool: false,
        mcp_servers: Some(json!({})),
        strict_mcp_config: true,
        setting_sources: Some(vec!["user".into(), "project".into(), "local".into()]),
        persist_session: false,
        additional_directories: vec![],
        settings: Some(json!({"disableAllHooks":true})),
        extra_args: vec![],
        append_system_prompt: None,
        system_prompt: Some(String::new()),
        supported_dialog_kinds: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn launch_matches_what_the_sdk_spawns_for_the_source_adapter_options() {
        let rows: Vec<Value> = serde_json::from_str(include_str!("../tests/fixtures/claude-launch.json")).unwrap();
        assert!(rows.len() >= 80);
        for row in &rows {
            let input = &row["input"];
            let environment: HashMap<String, String> = input["environment"].as_object().unwrap().iter().map(|(key, value)| (key.clone(), value.as_str().unwrap().to_owned())).collect();
            let mcp = input["mcp"].as_object().map(|_| Mcp { endpoint: input["mcp"]["endpoint"].as_str().unwrap(), authorization_header: input["mcp"]["authorizationHeader"].as_str().unwrap() });
            let cwd = std::path::PathBuf::from(input["cwd"].as_str().unwrap());
            let attachments = std::path::PathBuf::from(input["attachmentsDir"].as_str().unwrap());
            let options = query_options(LaunchInput {
                binary_path: input["settings"]["binaryPath"].as_str().unwrap(),
                launch_args: input["settings"]["launchArgs"].as_str().unwrap(),
                auto_compact_window: input["settings"]["autoCompactWindow"].as_str().unwrap(),
                environment,
                selection: &input["modelSelection"],
                native_thread_id: input["nativeThreadId"].as_str().unwrap(),
                resume: input["resume"] == true,
                resume_session_at: input["resumeSessionAt"].as_str(),
                fork_session: false,
                cwd: &cwd,
                attachments_dir: Some(&attachments),
                runtime_mode: input["runtimeMode"].as_str().unwrap(),
                interaction_mode: input["interactionMode"].as_str().unwrap(),
                mcp,
            });
            let context = format!("{input}");
            assert_eq!(json!(options.args()), row["args"], "{context}");
            assert_eq!(options.binary.to_str().unwrap(), row["command"].as_str().unwrap(), "{context}");
            assert_eq!(options.cwd.to_str().unwrap(), row["cwd"].as_str().unwrap(), "{context}");
            assert_eq!(json!(options.environment()), row["env"], "{context}");
            let mut initialize = options.initialize_request();
            let form = if input["mcp"].is_null() { "runtime" } else { "runtime+orchestration" };
            let append = initialize["appendSystemPrompt"].as_str().unwrap().to_owned();
            assert!(append.starts_with("<runtime_info>") && (form == "runtime" || append.contains("## T3 Code orchestration")), "{context}");
            assert_eq!(append.contains("## T3 Code orchestration"), form != "runtime", "{context}");
            initialize["appendSystemPrompt"] = json!(form);
            assert_eq!(initialize, row["initialize"], "{context}");
            let derived = policy(input["runtimeMode"].as_str().unwrap(), input["interactionMode"].as_str().unwrap());
            assert_eq!(derived.permission_mode, row["policy"]["permissionMode"].as_str().unwrap(), "{context}");
            assert_eq!(derived.ask_user, row["policy"]["installPermissionCallback"], "{context}");
        }
    }
    #[test]
    fn health_probe_matches_the_sdk_launch_of_the_source_probe() {
        let rows: Vec<Value> = serde_json::from_str(include_str!("../tests/fixtures/claude-probe-launch.json")).unwrap();
        assert!(!rows.is_empty());
        for row in rows {
            let environment: HashMap<String, String> = row["input"]["environment"].as_object().unwrap().iter().map(|(key, value)| (key.clone(), value.as_str().unwrap().to_owned())).collect();
            let options = probe_options(row["input"]["binaryPath"].as_str().unwrap(), Path::new(row["input"]["cwd"].as_str().unwrap()), environment);
            assert_eq!(json!(options.args()), row["args"]);
            assert_eq!(json!(options.environment()), row["env"]);
            assert_eq!(options.initialize_request(), row["initialize"]);
        }
    }
}

#[cfg(test)]
mod executable_tests {
    use super::*;
    use crate::provider_maintenance::Platform;

    const NPM_DIR: &str = r"C:\Users\dev\AppData\Roaming\npm";
    fn resolve(binary: &str, platform: Platform, resolved: Option<&str>, files: &[&str]) -> String {
        resolve_sdk_executable(binary, platform, &|_| resolved.map(str::to_owned), &|path| files.contains(&path))
    }

    #[test]
    fn a_bare_claude_follows_an_npm_launcher_shim_to_the_package_entry_on_windows() {
        let exe = format!(r"{NPM_DIR}\node_modules\@anthropic-ai\claude-code\bin\claude.exe");
        let cli = format!(r"{NPM_DIR}\node_modules\@anthropic-ai\claude-code\cli.js");
        // Other platforms are unchanged and never resolve.
        assert_eq!(resolve_sdk_executable("claude", Platform::Darwin, &|_| panic!("must not resolve"), &|_| false), "claude");
        // A native executable is returned as resolved.
        assert_eq!(resolve("claude", Platform::Win32, Some(r"C:\Users\dev\.local\bin\claude.exe"), &[]), r"C:\Users\dev\.local\bin\claude.exe");
        // A shim of any launcher extension and case follows to the native binary first.
        for shim in ["claude.cmd", "claude.bat", "claude.ps1", "claude.CMD"] {
            assert_eq!(resolve("claude", Platform::Win32, Some(&format!(r"{NPM_DIR}\{shim}")), &[exe.as_str(), cli.as_str()]), exe, "{shim}");
        }
        // Older packages ship only cli.js.
        assert_eq!(resolve("claude", Platform::Win32, Some(&format!(r"{NPM_DIR}\claude.cmd")), &[cli.as_str()]), cli);
        // No known entry, or nothing resolved, leaves the configured path.
        assert_eq!(resolve("claude", Platform::Win32, Some(&format!(r"{NPM_DIR}\claude.cmd")), &[]), "claude");
        assert_eq!(resolve("claude", Platform::Win32, None, &[]), "claude");
    }
}
