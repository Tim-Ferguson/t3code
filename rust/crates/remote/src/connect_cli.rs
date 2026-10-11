//! The logic behind `t3 connect` (`cli/connect.ts`): what each subcommand reads, writes and says.
//! The binary owns argument parsing, the terminal, and issuing the short-lived admin session used
//! to reach the running server; everything else is here.
//!
//! - `t3 connect [--headless]`: relay client + authorization + desired link + offer the service.
//! - `t3 connect login [--headless]`: authorize only ([`authorize_cli`]).
//! - `t3 connect link [--headless] [--publish-only]`: authorize and record the wish
//!   ([`link_environment_for_connect`]); the server links on its next start.
//! - `t3 connect publish [--disable]`: [`connect_publish`].
//! - `t3 connect status [--json]`: [`connect_status`] and [`format_cloud_status`].
//! - `t3 connect unlink` / `logout`: [`run_live_cloud_unlink`], [`unlink_relay_environment`] and
//!   [`report_cloud_disconnect_results`], after `clear_persisted_cloud_link` (logout also clears the token).

use std::time::Duration;

use serde::Serialize;

use crate::cli_state::*;
use crate::cli_token::{
    CliTokenError, CliTokenManager, DeviceAuthorizationPrompt, GetOutcome, LoginInteraction,
    PersistedToken,
};
use crate::control_plane::ControlPlane;
use crate::relay_client::{InstallProgressStage, RelayClientResolver, RelayClientStatus};
use crate::relay_error::RelayRequestError;
use crate::secrets::*;

pub const CLOUD_CLI_LIVE_SERVER_TIMEOUT: Duration = Duration::from_secs(5);

/// Inside an SSH session there is no local browser to complete the loopback OAuth callback, so the
/// device authorization grant is the only flow that can work (`headlessSessionConfig`).
pub fn is_headless_session(env: &dyn Fn(&str) -> Option<String>) -> bool {
    env("SSH_CONNECTION").is_some() || env("SSH_TTY").is_some()
}

pub fn format_device_authorization_prompt(prompt: &DeviceAuthorizationPrompt) -> String {
    let minutes = ((prompt.expires_in.as_secs_f64() / 60.0).round() as u64).max(1);
    [
        "Headless authorization".to_owned(),
        "Open this URL on a device with a browser:".to_owned(),
        format!(
            "  {}",
            prompt
                .verification_uri_complete
                .as_deref()
                .unwrap_or(&prompt.verification_uri)
        ),
        String::new(),
        format!("Confirm this code when asked: {}", prompt.user_code),
        String::new(),
        format!("Waiting for approval (expires in {minutes} min). Press Ctrl+C to cancel."),
    ]
    .join("\n")
}

/// Authorizes the CLI and returns the connected identity, if known (`authorizeCli`). Outside a
/// headless session the loopback flow runs first; pressing `H` (or `--headless`) switches to the
/// device flow. A stored credential whose refresh fails falls through to a fresh device flow.
pub async fn authorize_cli(
    tokens: &CliTokenManager,
    interaction: &dyn LoginInteraction,
    headless: bool,
    headless_session: bool,
    say: &dyn Fn(&str),
) -> Result<Option<String>, CliTokenError> {
    if !(headless || headless_session) {
        if let GetOutcome::Authorized(token) = tokens.get(interaction).await? {
            return Ok(token.identity);
        }
        say("\nHeadless mode enabled. A new authorization link is ready below.");
    }
    match tokens.get_existing().await {
        Ok(Some(token)) => return Ok(token.identity),
        Ok(None) => {}
        Err(CliTokenError::CredentialRefresh(_)) => {
            say("The stored T3 Connect credential could not be refreshed; signing in again.");
        }
        Err(other) => return Err(other),
    }
    let (token, identity) = tokens
        .device_authorization_login(|prompt| say(&format_device_authorization_prompt(prompt)))
        .await?;
    tokens.store(&token).await?;
    Ok(identity)
}

/// What `t3 connect status` shows: saved setup, not a live connection check.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudCliStatus {
    pub desired: bool,
    pub authenticated: bool,
    pub linked: bool,
    pub cloud_user_id: Option<String>,
    pub relay_url: Option<String>,
    pub publish_agent_activity: bool,
    pub relay_client: RelayClientStatus,
}

pub async fn connect_status(
    secrets: &dyn SecretStore,
    tokens: &CliTokenManager,
    relay_client: &dyn RelayClientResolver,
) -> Result<CloudCliStatus, String> {
    let desired = read_cli_desired_cloud_link(secrets).map_err(|e| e.message)?;
    let authenticated = tokens.has_credential().await.map_err(|e| e.to_string())?;
    let cloud_user_id = get_string(secrets, CLOUD_LINKED_USER_ID).map_err(|e| e.message)?;
    let relay_url = get_string(secrets, RELAY_URL_SECRET).map_err(|e| e.message)?;
    let publish = get_string(secrets, PUBLISH_AGENT_ACTIVITY_SECRET).map_err(|e| e.message)?;
    Ok(CloudCliStatus {
        desired,
        authenticated,
        linked: cloud_user_id.is_some(),
        cloud_user_id,
        relay_url,
        publish_agent_activity: is_agent_activity_publishing_enabled_value(publish.as_deref()),
        relay_client: relay_client.resolve().await,
    })
}

pub fn format_relay_client_status(status: &RelayClientStatus) -> Vec<String> {
    match status {
        RelayClientStatus::Available {
            executable_path,
            source,
            version,
        } => {
            let source = match source {
                crate::relay_client::ExecutableSource::Path => "PATH",
                crate::relay_client::ExecutableSource::Managed => "managed install",
                crate::relay_client::ExecutableSource::Override => "configured override",
            };
            vec![
                format!("  Relay client: available via {source}"),
                format!("    Path: {executable_path}"),
                format!("    Version: {version}"),
            ]
        }
        RelayClientStatus::Missing { .. } => vec!["  Relay client: not installed".to_owned()],
        RelayClientStatus::Unsupported {
            platform,
            arch,
            version,
        } => vec![
            format!("  Relay client: unsupported on {platform}-{arch}"),
            format!("    Managed version: {version}"),
        ],
    }
}

pub fn format_cloud_status(status: &CloudCliStatus, json: bool) -> String {
    if json {
        return serde_json::to_string_pretty(status).expect("status json");
    }
    let provisioned = if status.linked {
        "provisioned"
    } else if status.desired && status.authenticated {
        "pending server startup"
    } else {
        "not provisioned"
    };
    let next_step = if !status.authenticated {
        Some("Run `t3 connect link` to authorize and enable T3 Connect.")
    } else if !status.desired {
        Some("Run `t3 connect link` to enable T3 Connect.")
    } else if !status.linked {
        Some("Start T3 to provision the environment link and launch its managed tunnel.")
    } else {
        None
    };
    let mut lines = vec![
        "T3 Connect".to_owned(),
        format!(
            "  Exposure: {}",
            if status.desired {
                "enabled"
            } else {
                "disabled"
            }
        ),
        format!(
            "  Authorization: {}",
            if status.authenticated {
                "stored credential"
            } else {
                "missing"
            }
        ),
        format!("  Environment link: {provisioned}"),
        format!(
            "  Relay: {}",
            status.relay_url.as_deref().unwrap_or("not provisioned")
        ),
        format!(
            "  Publish agent activity: {}",
            if status.publish_agent_activity {
                "enabled"
            } else {
                "disabled"
            }
        ),
    ];
    lines.extend(format_relay_client_status(&status.relay_client));
    lines.push(String::new());
    lines.push("This is saved setup, not a live connection check. Check the background service with `t3 service status`.".to_owned());
    if let Some(next) = next_step {
        lines.push(String::new());
        lines.push(format!("Next: {next}"));
    }
    lines.join("\n")
}

pub fn relay_client_install_progress_message(stage: InstallProgressStage) -> &'static str {
    match stage {
        InstallProgressStage::Checking => "Checking existing installation",
        InstallProgressStage::WaitingForLock => "Waiting for installation lock",
        InstallProgressStage::Downloading => "Downloading",
        InstallProgressStage::Verifying => "Verifying download",
        InstallProgressStage::Installing => "Installing",
        InstallProgressStage::Validating => "Validating executable",
        InstallProgressStage::Activating => "Activating installation",
    }
}

/// `acquireRelayClientForLink`: use the client if present. An unsupported platform goes straight to
/// the installer (which reports why it cannot); a missing client is installed only after the user
/// confirms. `None`: the user declined.
pub async fn acquire_relay_client_for_link(
    relay_client: &dyn RelayClientResolver,
    confirm_install: &dyn Fn(&str) -> bool,
    report: &(dyn Fn(InstallProgressStage) + Send + Sync),
) -> Result<Option<RelayClientStatus>, crate::relay_client::InstallError> {
    match relay_client.resolve().await {
        status @ RelayClientStatus::Available { .. } => Ok(Some(status)),
        RelayClientStatus::Unsupported { .. } => {
            relay_client.install_with_progress(report).await.map(Some)
        }
        RelayClientStatus::Missing { version } => {
            if !confirm_install(&version) {
                return Ok(None);
            }
            relay_client.install_with_progress(report).await.map(Some)
        }
    }
}

/// Records the CLI's wish after authorization (`linkEnvironmentForConnect`, the part after the
/// relay client is ready). A publish-only link also turns publishing on.
pub fn link_environment_for_connect(
    secrets: &dyn SecretStore,
    publish_only: bool,
) -> Result<(), SecretError> {
    set_cli_desired_cloud_link(
        secrets,
        true,
        if publish_only {
            DesiredLinkMode::PublishOnly
        } else {
            DesiredLinkMode::Managed
        },
    )?;
    if publish_only {
        secrets.set(PUBLISH_AGENT_ACTIVITY_SECRET, b"true")?;
    }
    Ok(())
}

/// `t3 connect publish [--disable]`: the lines to print. Enabling without a link establishes a
/// tunnel-free publish-only link at the next start, when the CLI is signed in and no link is
/// pending; disabling also cancels a pending publish-only link.
pub async fn connect_publish(
    secrets: &dyn SecretStore,
    tokens: &CliTokenManager,
    disable: bool,
) -> Result<Vec<String>, String> {
    let io = |e: SecretError| e.message;
    let enabled = !disable;
    secrets
        .set(
            PUBLISH_AGENT_ACTIVITY_SECRET,
            if enabled { b"true" } else { b"false" },
        )
        .map_err(io)?;
    let mut out = Vec::new();
    if !enabled {
        // A pending managed link is left alone; it exists for the tunnel.
        let linked_now = secrets.get(CLOUD_LINKED_USER_ID).map_err(io)?.is_some();
        if !linked_now
            && read_cli_desired_link_mode(secrets).map_err(io)? == DesiredLinkMode::PublishOnly
        {
            set_cli_desired_cloud_link(secrets, false, DesiredLinkMode::Managed).map_err(io)?;
            out.push("Cancelled the pending publish-only T3 Connect link.".to_owned());
        }
        out.push("Publishing agent activity disabled.".to_owned());
        return Ok(out);
    }
    out.push("Publishing agent activity enabled.".to_owned());
    if secrets.get(CLOUD_LINKED_USER_ID).map_err(io)?.is_some() {
        return Ok(out);
    }
    if !tokens.has_credential().await.map_err(|e| e.to_string())? {
        out.push(
            "Run `t3 connect login` first so this environment can be authorized to publish."
                .to_owned(),
        );
        return Ok(out);
    }
    // Never downgrade a pending managed link: it also covers publishing.
    if read_cli_desired_cloud_link(secrets).map_err(io)? {
        out.push("A T3 Connect link is already pending. Start T3 to finish provisioning it; publishing starts once it links.".to_owned());
        return Ok(out);
    }
    set_cli_desired_cloud_link(secrets, true, DesiredLinkMode::PublishOnly).map_err(io)?;
    out.push("Restart T3 to finish authorizing this environment to publish (no managed tunnel is created).".to_owned());
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveCloudActionResult {
    NotRunning,
    Succeeded,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayUnlinkResult {
    NotAuthenticated,
    Revoked,
    NotLinked,
}

/// Asks the running server to unlink (`POST <origin>/api/connect/unlink` with a short-lived
/// session that has the relay-write scope). `session_token` is issued and revoked by the caller.
pub async fn run_live_cloud_unlink(
    origin: Option<&str>,
    session_token: &str,
) -> LiveCloudActionResult {
    let Some(origin) = origin else {
        return LiveCloudActionResult::NotRunning;
    };
    let call = reqwest::Client::new()
        .post(format!("{origin}/api/connect/unlink"))
        .bearer_auth(session_token)
        .send();
    match tokio::time::timeout(CLOUD_CLI_LIVE_SERVER_TIMEOUT, call).await {
        Ok(Ok(response)) if response.status().is_success() => LiveCloudActionResult::Succeeded,
        Ok(Ok(response)) => LiveCloudActionResult::Failed(format!("HTTP {}", response.status())),
        Ok(Err(error)) => LiveCloudActionResult::Failed(error.to_string()),
        Err(_) => LiveCloudActionResult::Failed("timed out".to_owned()),
    }
}

/// Deletes the relay-side environment record (`DELETE /v1/client/environment-links/:id`).
pub async fn unlink_relay_environment(
    control_plane: &dyn ControlPlane,
    token: Option<&PersistedToken>,
    relay_url: &str,
    environment_id: &str,
) -> Result<RelayUnlinkResult, RelayRequestError> {
    let Some(token) = token else {
        return Ok(RelayUnlinkResult::NotAuthenticated);
    };
    let response = control_plane
        .unlink_environment(relay_url, &token.access_token, environment_id)
        .await?;
    Ok(if response.ok {
        RelayUnlinkResult::Revoked
    } else {
        RelayUnlinkResult::NotLinked
    })
}

/// What `unlink` and `logout` print (`reportCloudDisconnectResults`).
pub fn report_cloud_disconnect_results(
    clear_authorization: bool,
    live: &LiveCloudActionResult,
    relay: &Result<RelayUnlinkResult, RelayRequestError>,
) -> Vec<String> {
    let mut out = Vec::new();
    if matches!(live, LiveCloudActionResult::Failed(_)) {
        out.push("T3 Connect is disabled, but the running server could not stop its tunnel.\nRestart that server to stop the connector.".to_owned());
    } else {
        out.push("T3 Connect is disabled locally.".to_owned());
    }
    match relay {
        Err(_) => out.push(
            if clear_authorization {
                "Could not revoke the relay-side environment record before signing out.\nThe stored CLI authorization was still removed locally."
            } else {
                "Could not revoke the relay-side environment record yet.\nRun `t3 connect unlink` again when the relay is reachable."
            }
            .to_owned(),
        ),
        Ok(RelayUnlinkResult::Revoked) => out.push("Revoked the relay-side environment record.".to_owned()),
        Ok(_) => {}
    }
    out
}

/// The final line of `t3 connect logout`.
pub const LOGOUT_NOTICE: &str = "Signed out of T3 Connect locally.\nThe background service is managed separately with `t3 service`.";

#[cfg(test)]
mod tests {
    use super::*;

    fn status(desired: bool, authenticated: bool, linked: bool) -> CloudCliStatus {
        CloudCliStatus {
            desired,
            authenticated,
            linked,
            cloud_user_id: linked.then(|| "user-1".to_owned()),
            relay_url: linked.then(|| "https://relay.example.test".to_owned()),
            publish_agent_activity: false,
            relay_client: RelayClientStatus::Missing {
                version: "2026.5.2".into(),
            },
        }
    }

    #[test]
    fn status_text_names_the_next_step() {
        assert!(
            format_cloud_status(&status(false, false, false), false)
                .contains("Next: Run `t3 connect link` to authorize and enable T3 Connect.")
        );
        assert!(
            format_cloud_status(&status(false, true, false), false)
                .contains("Next: Run `t3 connect link` to enable T3 Connect.")
        );
        let pending = format_cloud_status(&status(true, true, false), false);
        assert!(pending.contains("Environment link: pending server startup"));
        assert!(pending.contains("Next: Start T3 to provision"));
        let linked = format_cloud_status(&status(true, true, true), false);
        assert!(linked.contains("Environment link: provisioned"));
        assert!(!linked.contains("Next:"));
        assert!(linked.contains("Relay client: not installed"));
    }

    #[test]
    fn status_json_uses_camel_case() {
        let json: serde_json::Value =
            serde_json::from_str(&format_cloud_status(&status(true, true, true), true)).unwrap();
        assert_eq!(json["cloudUserId"], "user-1");
        assert_eq!(json["relayClient"]["status"], "missing");
    }

    #[test]
    fn device_prompt_prefers_the_complete_uri_and_rounds_minutes() {
        let prompt = DeviceAuthorizationPrompt {
            verification_uri: "https://accounts.example.test/device".into(),
            verification_uri_complete: Some(
                "https://accounts.example.test/device?user_code=BCDF-GHJK".into(),
            ),
            user_code: "BCDF-GHJK".into(),
            expires_in: Duration::from_secs(600),
        };
        let text = format_device_authorization_prompt(&prompt);
        assert!(text.contains("  https://accounts.example.test/device?user_code=BCDF-GHJK"));
        assert!(text.contains("Confirm this code when asked: BCDF-GHJK"));
        assert!(text.contains("expires in 10 min"));
    }

    #[test]
    fn link_for_connect_records_the_wish_and_publish_opt_in() {
        let store = MemorySecretStore::new();
        link_environment_for_connect(&store, false).unwrap();
        assert_eq!(
            read_cli_desired_link_mode(&store).unwrap(),
            DesiredLinkMode::Managed
        );
        assert!(store.get(PUBLISH_AGENT_ACTIVITY_SECRET).unwrap().is_none());
        link_environment_for_connect(&store, true).unwrap();
        assert_eq!(
            read_cli_desired_link_mode(&store).unwrap(),
            DesiredLinkMode::PublishOnly
        );
        assert_eq!(
            store.text(PUBLISH_AGENT_ACTIVITY_SECRET).as_deref(),
            Some("true")
        );
    }

    #[test]
    fn disconnect_reports() {
        let ok = report_cloud_disconnect_results(
            false,
            &LiveCloudActionResult::Succeeded,
            &Ok(RelayUnlinkResult::Revoked),
        );
        assert_eq!(
            ok,
            [
                "T3 Connect is disabled locally.",
                "Revoked the relay-side environment record."
            ]
        );
        let failed = report_cloud_disconnect_results(
            true,
            &LiveCloudActionResult::Failed("x".into()),
            &Err(RelayRequestError::unexpected_response()),
        );
        assert!(failed[0].starts_with("T3 Connect is disabled, but the running server"));
        assert!(failed[1].contains("before signing out"));
        let not_running = report_cloud_disconnect_results(
            false,
            &LiveCloudActionResult::NotRunning,
            &Ok(RelayUnlinkResult::NotAuthenticated),
        );
        assert_eq!(not_running, ["T3 Connect is disabled locally."]);
    }
}
