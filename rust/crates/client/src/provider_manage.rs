//! What the provider settings page says about keeping a provider current and installed: its
//! version, the advisory and the update it offers, the managed Codex runtime, ACP registry
//! sessions and providers, and the words for each failure. Port of `providerStatus.ts`,
//! `ProviderUpdateLaunchNotification.logic.ts`, `ProviderSetupSection.tsx`,
//! `CodexSetupSection.tsx`, `AcpSessionManagementSection.tsx` and
//! `AddProviderInstanceDialog.logic.ts`. Providers are the snapshots the server publishes,
//! read as JSON like the rest of the settings page.
use serde_json::{Value, json};

pub const REFRESH: &str = "server.refreshProviders";
pub const UPDATE: &str = "server.updateProvider";
pub const INSTALL_SUBSCRIBE: &str = "provider.install.subscribe";
pub const INSTALL_START: &str = "provider.install.start";
pub const INSTALL_CANCEL: &str = "provider.install.cancel";
pub const INSTALL_REMOVE: &str = "provider.install.remove";
pub const LIST_SESSIONS: &str = "server.listAcpRegistrySessions";
pub const IMPORT_SESSION: &str = "server.importAcpRegistrySession";
pub const DELETE_SESSION: &str = "server.deleteAcpRegistrySession";
pub const LIST_PROVIDERS: &str = "server.listAcpRegistryProviders";
pub const SET_PROVIDER: &str = "server.setAcpRegistryProvider";
pub const DISABLE_PROVIDER: &str = "server.disableAcpRegistryProvider";
pub const LOGOUT: &str = "server.logoutAcpRegistry";

/// The version as a row shows it: a bare version gets a `v`, Antigravity's release tag its date.
pub fn version_label(version: Option<&str>) -> Option<String> {
    let version = version.filter(|version| !version.is_empty())?;
    if let Some(rest) = version.strip_prefix("agy_acp_server_") {
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        let tail = &rest[digits.len()..];
        if digits.len() == 8 && tail.starts_with('_') {
            let mut parts = tail[1..].splitn(2, '_');
            let build = parts.next().unwrap_or_default();
            let candidate = parts.next();
            let word = |text: &str| !text.is_empty() && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            if !build.is_empty() && build.chars().all(|c| c.is_ascii_digit()) && candidate.is_none_or(word) {
                let date = format!("{}-{}-{}", &digits[..4], &digits[4..6], &digits[6..]);
                return Some(match candidate {
                    Some(candidate) => format!("{date} {candidate}"),
                    None => date,
                });
            }
        }
    }
    Some(if version.chars().next().is_some_and(|c| c.is_ascii_digit()) { format!("v{version}") } else { version.to_owned() })
}

/// What the version popover says, and what it offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advisory {
    pub title: String,
    pub detail: String,
    pub update_command: Option<String>,
    /// Compatibility trouble reads in a stronger colour than "an update exists".
    pub strong: bool,
    /// A version the server recommends installing instead of the latest.
    pub target_version: Option<String>,
    pub compatibility_warning: bool,
}

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value[key].as_str().filter(|text| !text.is_empty())
}

/// `getProviderVersionAdvisoryPresentation`, with compatibility shown while the provider is enabled.
pub fn advisory(provider: &Value) -> Option<Advisory> {
    let enabled = provider["enabled"] != false;
    let (version, compatibility) = (&provider["versionAdvisory"], &provider["compatibilityAdvisory"]);
    let latest_incompatible = matches!(compatibility["latestVersionStatus"].as_str(), Some("broken" | "unsupported"));
    if enabled && compatibility.is_object() {
        if let Some(status) = compatibility["status"].as_str().filter(|status| matches!(*status, "graceful" | "unsupported" | "broken")) {
            let target = text(compatibility, "recommendedVersion");
            let recommendation = version_label(target).or_else(|| text(compatibility, "recommendedRange").map(str::to_owned));
            let title = match status {
                "graceful" => "Limited support",
                "unsupported" => "Unsupported version",
                _ => "Known broken version",
            };
            return Some(Advisory {
                title: title.into(),
                detail: text(compatibility, "message").map(str::to_owned).unwrap_or_else(|| match recommendation {
                    Some(recommendation) => format!("Use {recommendation} for full support."),
                    None => "Update for full support.".into(),
                }),
                update_command: if target.is_some() || latest_incompatible { None } else { text(version, "updateCommand").map(str::to_owned) },
                strong: status != "graceful",
                target_version: target.map(str::to_owned),
                compatibility_warning: true,
            });
        }
    }
    if !version.is_object() || matches!(version["status"].as_str(), None | Some("current" | "unknown")) || latest_incompatible {
        return None;
    }
    let label = "Update available";
    let detail = text(version, "message").map(str::to_owned).unwrap_or_else(|| match version_label(text(version, "latestVersion")) {
        Some(version) => format!("{label}: install {version}."),
        None => format!("{label}: install the latest provider version."),
    });
    Some(Advisory { title: label.into(), detail, update_command: text(version, "updateCommand").map(str::to_owned), strong: false, target_version: None, compatibility_warning: false })
}

/// `isProviderSettingsUpdateCandidate`: an enabled provider behind its latest version that the
/// server can update with the command it shows.
pub fn is_update_candidate(provider: &Value) -> bool {
    provider["enabled"] != false
        && !matches!(provider["compatibilityAdvisory"]["latestVersionStatus"].as_str(), Some("broken" | "unsupported"))
        && provider["versionAdvisory"]["status"] == "behind_latest"
        && provider["versionAdvisory"]["canUpdate"] == true
        && text(&provider["versionAdvisory"], "updateCommand").is_some()
}

/// `isProviderUpdateActive`: the server has the update queued or is running it.
pub fn update_active(provider: &Value) -> bool {
    matches!(provider["updateState"]["status"].as_str(), Some("queued" | "running"))
}

/// Installing the version compatibility recommends, rather than the latest.
pub fn can_install_recommended(provider: &Value) -> bool {
    text(&provider["compatibilityAdvisory"], "message").is_some() && text(&provider["compatibilityAdvisory"], "recommendedVersion").is_some() && provider["versionAdvisory"]["canInstallVersion"] == true
}

/// How the last update ended, in the words the original's toasts use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateOutcome {
    pub running: bool,
    pub failed: bool,
    pub title: String,
    pub detail: Option<String>,
    pub output: Option<String>,
}
pub fn update_outcome(provider: &Value) -> Option<UpdateOutcome> {
    let state = &provider["updateState"];
    let message = text(state, "message").map(str::to_owned);
    let output = text(state, "output").map(str::to_owned);
    let outcome = |running, failed, title: &str, detail: Option<String>| Some(UpdateOutcome { running, failed, title: title.into(), detail, output: output.clone() });
    match state["status"].as_str()? {
        "queued" | "running" => outcome(true, false, "Updating provider", Some("Running provider update command.".into())),
        "failed" => outcome(false, true, "Provider update failed", message.or_else(|| Some("Check provider settings for details.".into()))),
        "unchanged" => outcome(false, true, "Provider still needs an update", message.or_else(|| Some("The provider still appears outdated.".into()))),
        "succeeded" => outcome(false, false, "Provider updated", Some("New sessions will use the updated provider.".into())),
        _ => None,
    }
}

/// The input of `server.updateProvider`.
pub fn update_input(driver: &str, instance_id: &str, target_version: Option<&str>) -> Value {
    let mut input = json!({"provider":driver,"instanceId":instance_id});
    if let Some(version) = target_version {
        input["targetVersion"] = json!(version);
    }
    input
}

/// The question asked before a package manager runs on the server's machine: what runs, and where.
pub fn update_question(name: &str, environment: &str, command: Option<&str>, target: Option<&str>) -> (String, String) {
    match (command, target) {
        (Some(command), _) => (format!("Update {name}?"), format!("This runs `{command}` on {environment}.")),
        (None, Some(target)) => (
            format!("Install {} for {name}?", version_label(Some(target)).unwrap_or_else(|| target.to_owned())),
            format!("{environment} runs its package manager to install that version of {name}."),
        ),
        (None, None) => (format!("Update {name}?"), format!("{environment} runs the provider's package manager.")),
    }
}

/// What the page last checked, as the original's "Checked …" row says it, in UTC so that it does
/// not go out of date while the page is open.
pub fn checked_label(providers: &[Value]) -> Option<String> {
    let latest = providers.iter().filter_map(|provider| provider["checkedAt"].as_str()).max()?;
    Some(match chrono::DateTime::parse_from_rfc3339(latest) {
        Ok(at) => format!("Checked {} UTC", at.with_timezone(&chrono::Utc).format("%H:%M")),
        Err(_) => "Checked unavailable".into(),
    })
}

// ---- the managed runtime ----------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallAction {
    None,
    Cancel,
    Install,
}
#[derive(Debug, Clone, PartialEq)]
pub struct InstallView {
    pub status: String,
    /// Bytes downloaded and in total, while a download of known size runs.
    pub progress: Option<(u64, u64)>,
    pub action: InstallAction,
    pub action_label: String,
    pub active: bool,
    pub can_remove: bool,
    pub operation_id: Option<String>,
    /// Said beside the status when it is not already the status.
    pub message: Option<String>,
}
fn megabytes(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / 1_000_000.0)
}
/// The runtime row of a provider the server can install, from its install state. `None` is the state
/// before the first answer.
pub fn install_view(install: Option<&Value>, provider: &Value, name: &str, custom_binary: bool) -> InstallView {
    let phase = install.and_then(|install| install["phase"].as_str());
    let active = matches!(phase, Some("downloading" | "extracting" | "verifying"));
    let installed_version = install.and_then(|install| text(install, "installedVersion"));
    let installed = provider["installed"] == true || (!custom_binary && installed_version.is_some());
    let downloaded = install.map_or(0, |install| install["downloadedBytes"].as_u64().unwrap_or(0));
    let total = install.and_then(|install| install["totalBytes"].as_u64());
    let status = match phase {
        Some("downloading") => format!("Downloading {}{} MB.", megabytes(downloaded), total.map_or(String::new(), |total| format!(" of {}", megabytes(total)))),
        Some("extracting") => format!("Installing {name}."),
        Some("verifying") => format!("Checking {name}."),
        _ if installed => format!(
            "{}{}.",
            if install.is_some_and(|install| install["source"] == "local") { format!("Using your installed {name}") } else { "Managed by T3 Code".to_owned() },
            installed_version.map_or(String::new(), |version| format!(" \u{b7} v{version}"))
        ),
        _ if custom_binary => format!("The configured {name} runtime has not been checked."),
        _ => install.and_then(|install| install["message"].as_str()).filter(|message| !message.is_empty()).map_or_else(|| format!("T3 Code downloads and manages {name} for you."), str::to_owned),
    };
    let target_differs = install.is_some_and(|install| text(install, "version").is_some_and(|version| Some(version) != installed_version));
    let action_label = if installed_version.is_some() {
        if target_differs { format!("Update {name}") } else { format!("Reinstall {name}") }
    } else if matches!(phase, Some("failed" | "cancelled")) {
        "Retry installation".to_owned()
    } else if installed {
        "Install managed runtime".to_owned()
    } else {
        format!("Install {name}")
    };
    let operation_id = install.and_then(|install| text(install, "operationId")).map(str::to_owned);
    let can_install = provider["setup"]["canInstall"] == true;
    let action = if active && operation_id.is_some() {
        InstallAction::Cancel
    } else if !active && can_install {
        InstallAction::Install
    } else {
        InstallAction::None
    };
    let message = install.and_then(|install| text(install, "message")).filter(|message| !active && *message != status).map(str::to_owned);
    InstallView {
        progress: (phase == Some("downloading")).then_some(()).and(total).filter(|total| *total > 0).map(|total| (downloaded, total)),
        status,
        action,
        action_label,
        active,
        can_remove: install.is_some_and(|install| install["canRemove"] == true) && !active,
        operation_id,
        message,
    }
}

/// Said when the server cannot install a provider and there is no custom binary to use instead.
pub const INSTALL_UNAVAILABLE: &str = "Automatic installation unavailable. Set a binary path or use another environment.";
pub const KEEPS_CUSTOM_BINARY: &str = "Uses the custom binary path below. Installation keeps that path.";

pub fn remove_question(name: &str, environment: &str) -> String {
    format!("Remove the downloaded {name} runtime from {environment}? Thread history is kept.")
}

// ---- failures ---------------------------------------------------------------------------------

/// The words for a failed management request, from the error the server answered with: `None` for
/// an error this module does not know, which the caller reads like any other.
pub fn failure_message(cause: &Value) -> Option<String> {
    let error = cause.as_array().and_then(|causes| causes.iter().find_map(|cause| cause.get("error"))).unwrap_or(cause);
    match error["_tag"].as_str()? {
        "ServerProviderUpdateError" => Some(format!("Provider update failed for {}: {}", error["provider"].as_str()?, error["reason"].as_str()?)),
        "ProviderSetupError" => error["detail"].as_str().map(str::to_owned),
        "AcpRegistryOperationError" => Some(error["message"].as_str().filter(|message| !message.is_empty()).map_or_else(|| acp_reason(error["reason"].as_str().unwrap_or_default()).to_owned(), str::to_owned)),
        _ => None,
    }
}
/// The sentence for an ACP registry failure whose message came back empty, switched on `reason`.
fn acp_reason(reason: &str) -> &'static str {
    match reason {
        "session_list_unsupported" => "This agent cannot list its sessions.",
        "session_resume_unsupported" => "This agent cannot resume its sessions.",
        "session_import_failed" => "The session could not be imported.",
        "session_delete_unsupported" => "This agent cannot delete its sessions.",
        "session_delete_failed" => "The session could not be deleted.",
        "providers_unsupported" => "This agent has no configurable providers.",
        "providers_list_failed" => "The agent's providers could not be listed.",
        "provider_configuration_failed" => "The provider could not be configured.",
        "logout_unsupported" => "This agent cannot log out.",
        "logout_failed" => "The agent could not log out.",
        "authentication_failed" => "The agent could not sign in.",
        "instance_not_found" => "That provider instance no longer exists.",
        "project_not_found" => "That project no longer exists.",
        _ => "The ACP operation failed.",
    }
}

// ---- ACP registry -------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AcpAbilities {
    pub list: bool,
    pub import: bool,
    pub delete: bool,
    pub configure_providers: bool,
    pub logout: bool,
}
pub fn acp_abilities(provider: &Value) -> AcpAbilities {
    let sessions = &provider["nativeSessions"];
    AcpAbilities {
        list: sessions["canList"] == true,
        import: sessions["canLoad"] == true || sessions["canResume"] == true,
        delete: sessions["canDelete"] == true,
        configure_providers: provider["configurableProviders"] == true,
        logout: provider["auth"]["canLogout"] == true && provider["setup"]["canAuthenticate"] != true,
    }
}
impl AcpAbilities {
    /// The section is shown at all.
    pub fn any(&self) -> bool {
        self.list || self.configure_providers || self.logout
    }
}

/// A page of sessions after the ones already listed: the first page replaces the list.
pub fn merge_sessions(current: Vec<Value>, page: &Value, append: bool) -> Vec<Value> {
    let incoming = page["sessions"].as_array().cloned().unwrap_or_default();
    if append { current.into_iter().chain(incoming).collect() } else { incoming }
}

pub fn delete_session_question(session: &Value) -> String {
    let name = session["title"].as_str().or_else(|| session["sessionId"].as_str()).unwrap_or_default();
    format!("Permanently delete native ACP session \"{name}\"?")
}
pub fn disable_provider_question(provider_id: &str) -> String {
    format!("Disable ACP provider \"{provider_id}\"?")
}

/// What is typed for one configurable provider.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProviderDraft {
    pub api_type: String,
    pub base_url: String,
    pub headers: String,
}
pub fn provider_draft(provider: &Value) -> ProviderDraft {
    ProviderDraft {
        api_type: provider["current"]["apiType"].as_str().or_else(|| provider["supported"][0].as_str()).unwrap_or_default().to_owned(),
        base_url: provider["current"]["baseUrl"].as_str().unwrap_or_default().to_owned(),
        headers: String::new(),
    }
}
/// The input that configures a provider, or the toast title and text that refuse it. Headers are a
/// JSON object of strings and are never read back from the server.
pub fn set_provider_input(instance_id: &str, project_id: &str, provider_id: &str, draft: &ProviderDraft) -> Result<Value, (String, String)> {
    let mut input = json!({"instanceId":instance_id,"projectId":project_id,"providerId":provider_id,"apiType":draft.api_type,"baseUrl":draft.base_url});
    if !draft.headers.trim().is_empty() {
        let invalid = |text: &str| ("Invalid provider headers".to_owned(), text.to_owned());
        let parsed: Value = serde_json::from_str(&draft.headers).map_err(|error| invalid(&error.to_string()))?;
        if !parsed.as_object().is_some_and(|headers| headers.values().all(Value::is_string)) {
            return Err(invalid("Headers must be a JSON object with string values."));
        }
        input["headers"] = parsed;
    }
    Ok(input)
}

// ---- a new instance ---------------------------------------------------------------------------

/// The instance id a label suggests, `driver_slug`: empty while the label has no letters or digits.
pub fn derive_instance_id(driver: &str, label: &str) -> String {
    let mut slug = String::new();
    let mut gap = false;
    for c in label.trim().to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            if gap && !slug.is_empty() {
                slug.push('_');
            }
            gap = false;
            slug.push(c);
        } else {
            gap = true;
        }
    }
    slug.truncate(48);
    if slug.is_empty() { String::new() } else { format!("{driver}_{slug}") }
}
/// The suggestion, numbered when it is taken.
pub fn available_instance_id(driver: &str, label: &str, existing: &dyn Fn(&str) -> bool) -> String {
    let base = derive_instance_id(driver, label);
    if base.is_empty() || !existing(&base) {
        return base;
    }
    (2..)
        .map(|n| {
            let suffix = format!("_{n}");
            format!("{}{suffix}", &base[..base.len().min(64 - suffix.len())])
        })
        .find(|candidate| !existing(candidate))
        .expect("an unbounded range")
}
pub fn validate_instance_id(id: &str, existing: &dyn Fn(&str) -> bool) -> Option<String> {
    if id.is_empty() {
        return Some("Instance ID is required.".into());
    }
    if id.chars().count() > 64 {
        return Some("Instance ID must be 64 characters or fewer.".into());
    }
    let mut chars = id.chars();
    if !chars.next().is_some_and(|c| c.is_ascii_alphabetic()) || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Some("Instance ID must start with a letter and use only letters, digits, '-', or '_'.".into());
    }
    existing(id).then(|| format!("An instance named '{id}' already exists."))
}
/// The settings change that adds an instance.
pub fn create_input(instance_id: &str, driver: &str, label: &str, config: &Value) -> Value {
    let mut instance = json!({"driver":driver,"enabled":true});
    if !label.trim().is_empty() {
        instance["displayName"] = json!(label.trim());
    }
    if config.as_object().is_some_and(|config| !config.is_empty()) {
        instance["config"] = config.clone();
    }
    json!({"patch":{},"providerInstanceMutation":{"operation":"create","instanceId":instance_id,"instance":instance}})
}

/// The launch arguments of a local ACP command, as the rows the editor shows.
pub fn command_arguments(config: &Value) -> Vec<String> {
    config["commandArgs"].as_array().map(|args| args.iter().filter_map(|arg| arg.as_str().map(str::to_owned)).collect()).unwrap_or_default()
}
/// The config with these arguments, one literal argument per row, in launch order.
pub fn with_command_arguments(config: &Value, args: &[String]) -> Value {
    let mut next = if config.is_object() { config.clone() } else { json!({}) };
    next["commandArgs"] = json!(args);
    next
}
