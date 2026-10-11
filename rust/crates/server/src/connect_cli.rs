//! `t3-server connect [--headless] | login | link [--publish-only] | publish [--disable] | status [--json] | unlink | logout`
//! (`apps/server/src/cli/connect.ts`). What each prints, reads and writes is `t3_remote::connect_cli`; this file runs them against
//! the state directory: its secret store, the CLI's stored sign-in, the relay client, and (for `unlink`/`logout`) a short-lived
//! session with which to ask a running server to stop its tunnel.
//!
//! Not ported: the offer to install the background service after `t3 connect` (`t3 service` has no counterpart here), so the
//! command ends with how to start the server, as the original does when the offer is declined or fails.
use std::{
    io::{BufRead, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use serde_json::Value;
use t3_contracts::AuthEnvironmentScope;
use t3_remote::{
    cli_state::{DesiredLinkMode, clear_persisted_cloud_link, set_cli_desired_cloud_link},
    cli_token::{CliTokenError, CliTokenManager, LoginAction, LoginInteraction},
    clock::{Clock, SystemClock},
    config::{BuildDefaults, cloud_cli_oauth_config, hosted_app_url_config, relay_url_config, DEFAULT_HOSTED_APP_URL},
    connect_cli as logic,
    control_plane::{ControlPlane, HttpControlPlane},
    relay_client::{Cloudflared, CloudflaredOptions, HostPlatform, RelayClientResolver, RelayClientStatus},
    relay_error::RelayRequestError,
};

use crate::{
    connect::{ConnectSecrets, base_dir_of},
    server_secret_store::ServerSecretStore,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subcommand {
    Connect,
    Login,
    Link,
    Publish,
    Status,
    Unlink,
    Logout,
}

impl Subcommand {
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "login" => Self::Login,
            "link" => Self::Link,
            "publish" => Self::Publish,
            "status" => Self::Status,
            "unlink" => Self::Unlink,
            "logout" => Self::Logout,
            _ => return None,
        })
    }
}

/// The flags of the subcommands; each reads the ones it has.
#[derive(Debug, Clone, Copy, Default)]
pub struct Flags {
    pub headless: bool,
    pub publish_only: bool,
    pub disable: bool,
    pub json: bool,
}

/// Everything a run touches outside the state directory, so a test replaces the relay, the terminal and the environment.
pub struct CliEnvironment {
    pub state_dir: PathBuf,
    pub env: Arc<dyn Fn(&str) -> Option<String> + Send + Sync>,
    pub interaction: Arc<dyn LoginInteraction>,
    /// Asked before downloading the relay client; the argument is the version.
    pub confirm_install: Arc<dyn Fn(&str) -> bool + Send + Sync>,
    pub out: Arc<dyn Fn(&str) + Send + Sync>,
    pub warn: Arc<dyn Fn(&str) + Send + Sync>,
    pub control_plane: Arc<dyn ControlPlane>,
    pub relay_client: Arc<dyn RelayClientResolver>,
    pub clock: Arc<dyn Clock>,
    /// Replace the OAuth application and the relay URL that the environment would configure. A normal run leaves both `None`;
    /// a test points them at loopback servers, which the secure-origin rule for a configured relay would refuse.
    pub oauth: Option<t3_remote::config::CloudCliOAuthConfig>,
    pub relay_url: Option<String>,
}

impl CliEnvironment {
    pub fn production(state_dir: PathBuf) -> Self {
        let base_dir = base_dir_of(&state_dir);
        let env = |name: &str| std::env::var(name).ok();
        Self {
            relay_client: Arc::new(Cloudflared::new(CloudflaredOptions {
                base_dir,
                host: HostPlatform::current(),
                executable_override: env("T3CODE_CLOUDFLARED_PATH"),
                path_env: env("PATH"),
                release_asset: None,
            })),
            state_dir,
            env: Arc::new(|name| std::env::var(name).ok()),
            interaction: Arc::new(TerminalInteraction),
            confirm_install: Arc::new(|version| confirm(&format!("The T3 relay client is required for T3 Connect. Download and install version {version}? [y/N] "))),
            out: Arc::new(|text| println!("{text}")),
            warn: Arc::new(|text| eprintln!("{text}")),
            control_plane: Arc::new(HttpControlPlane::new()),
            clock: Arc::new(SystemClock),
            oauth: None,
            relay_url: None,
        }
    }
}

fn confirm(question: &str) -> bool {
    print!("{question}");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer).is_ok() && matches!(answer.trim().to_lowercase().as_str(), "y" | "yes")
}

/// The terminal of a person signing in: a line of input stands for the key press (an empty line opens the browser, `h` goes
/// headless), as there is no raw terminal mode here.
pub struct TerminalInteraction;

#[async_trait]
impl LoginInteraction for TerminalInteraction {
    fn show_authorization_url(&self, authorization_url: &str) {
        println!("Open this URL to authorize T3 Connect:\n  {authorization_url}\n\nPress Enter to open it in your browser, or type h and press Enter to authorize without a local browser.");
    }

    async fn next_action(&self) -> LoginAction {
        loop {
            let line = tokio::task::spawn_blocking(|| {
                let mut line = String::new();
                std::io::stdin().lock().read_line(&mut line).map(|read| (read, line))
            })
            .await;
            match line {
                Ok(Ok((0, _))) | Ok(Err(_)) | Err(_) => return std::future::pending().await,
                Ok(Ok((_, line))) => match line.trim().to_lowercase().as_str() {
                    "" => return LoginAction::OpenBrowser,
                    "h" => return LoginAction::Headless,
                    _ => continue,
                },
            }
        }
    }

    async fn launch_browser(&self, url: &str) -> Result<(), String> {
        let (program, arguments): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
            ("open", vec![url])
        } else if cfg!(windows) {
            ("cmd", vec!["/C", "start", "", url])
        } else {
            ("xdg-open", vec![url])
        };
        std::process::Command::new(program).args(arguments).spawn().map(|_| ()).map_err(|error| error.to_string())
    }

    fn warn(&self, message: &str) {
        eprintln!("{message}");
    }
}

fn connected_as(identity: Option<&str>) -> String {
    identity.map(|identity| format!(" as {identity}")).unwrap_or_default()
}

struct Context<'a> {
    io: &'a CliEnvironment,
    secrets: Arc<ConnectSecrets>,
    tokens: Arc<CliTokenManager>,
}

impl Context<'_> {
    fn say(&self, text: &str) {
        (self.io.out)(text);
    }
}

fn open(io: &CliEnvironment) -> Result<Context<'_>, String> {
    let secrets = Arc::new(ConnectSecrets(ServerSecretStore::open(io.state_dir.join("secrets")).map_err(|error| error.to_string())?));
    let defaults = BuildDefaults::default();
    let env = |name: &str| (io.env)(name);
    // An unconfigured Clerk application cannot sign in or refresh; its endpoints are empty, so nothing is requested.
    let oauth = io.oauth.clone().unwrap_or_else(|| cloud_cli_oauth_config(&env, &defaults).unwrap_or_else(|_| t3_remote::config::CloudCliOAuthConfig::unconfigured()));
    let hosted_app_url = hosted_app_url_config(&env).unwrap_or_else(|_| DEFAULT_HOSTED_APP_URL.to_owned());
    let tokens = Arc::new(CliTokenManager::new(secrets.clone(), io.clock.clone(), oauth, hosted_app_url));
    Ok(Context { io, secrets, tokens })
}

/// `linkEnvironmentForConnect`: the relay client (unless publish-only), the sign-in, and the wish to link at the next start.
/// `None` when the person declined to install the relay client.
async fn link_environment(context: &Context<'_>, flags: Flags, publish_only: bool) -> Result<Option<Option<String>>, String> {
    if !publish_only {
        let report = |stage| context.say(&format!("Relay client: {}...", logic::relay_client_install_progress_message(stage)));
        let confirm = |version: &str| (context.io.confirm_install)(version);
        match logic::acquire_relay_client_for_link(context.io.relay_client.as_ref(), &confirm, &report).await.map_err(|error| error.message)? {
            None => {
                context.say("T3 Connect setup cancelled. The relay client was not installed.");
                return Ok(None);
            }
            Some(RelayClientStatus::Available { version, .. }) => context.say(&format!("✓ Relay client ready · cloudflared {version}")),
            Some(_) => {}
        }
    }
    let headless_session = logic::is_headless_session(context.io.env.as_ref());
    let say = |text: &str| context.say(text);
    let identity = logic::authorize_cli(&context.tokens, context.io.interaction.as_ref(), flags.headless, headless_session, &say).await.map_err(|error| error.to_string())?;
    logic::link_environment_for_connect(context.secrets.as_ref(), publish_only).map_err(|error| error.message)?;
    Ok(Some(identity))
}

const SERVE: &str = "t3-server serve";

pub async fn run(subcommand: Subcommand, flags: Flags, io: &CliEnvironment) -> Result<(), String> {
    let context = open(io)?;
    match subcommand {
        Subcommand::Connect => {
            context.say("T3 Connect\n");
            let Some(identity) = link_environment(&context, flags, false).await? else { return Ok(()) };
            // Show which account was linked so an unexpected identity (an authorization code for a different account) is
            // visible before the machine is brought online.
            context.say(&format!("✓ Authorized{}", connected_as(identity.as_deref())));
            context.say(&format!("\nNext\n  Start the server with `{SERVE}` to make this machine reachable."));
        }
        Subcommand::Login => {
            context.say("T3 Connect\n");
            let headless_session = logic::is_headless_session(io.env.as_ref());
            let say = |text: &str| context.say(text);
            let identity = logic::authorize_cli(&context.tokens, io.interaction.as_ref(), flags.headless, headless_session, &say).await.map_err(|error| error.to_string())?;
            context.say(&format!("✓ Signed in{}", connected_as(identity.as_deref())));
        }
        Subcommand::Link => {
            context.say("T3 Connect\n");
            if let Some(identity) = link_environment(&context, flags, flags.publish_only).await? {
                let who = connected_as(identity.as_deref());
                context.say(&if flags.publish_only {
                    format!("✓ Authorized{who}\n\nNext\n  Start T3 to publish agent activity (no managed tunnel).")
                } else {
                    format!("✓ Authorized{who}\n\nNext\n  Start the server with `{SERVE}` to make this machine reachable.")
                });
            }
        }
        Subcommand::Status => {
            let status = logic::connect_status(context.secrets.as_ref(), &context.tokens, io.relay_client.as_ref()).await?;
            context.say(&logic::format_cloud_status(&status, flags.json));
        }
        Subcommand::Publish => {
            for line in logic::connect_publish(context.secrets.as_ref(), &context.tokens, flags.disable).await? {
                context.say(&line);
            }
        }
        Subcommand::Unlink => disconnect(&context, false).await?,
        Subcommand::Logout => disconnect(&context, true).await?,
    }
    Ok(())
}

/// What the running server writes about itself (`server-runtime.json`, `serverRuntimeState.ts`): where to reach it.
pub fn runtime_state_path(state_dir: &Path) -> PathBuf {
    state_dir.join("server-runtime.json")
}

fn running_origin(state_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(runtime_state_path(state_dir)).ok()?;
    let state: Value = serde_json::from_str(text.trim()).ok()?;
    (state["version"] == 1).then(|| state["origin"].as_str().map(str::to_owned))?
}

/// Asks the running server to unlink with a session that lasts as long as the request needs (`withCloudCliSessionToken`).
async fn live_unlink(context: &Context<'_>) -> logic::LiveCloudActionResult {
    let state_dir = &context.io.state_dir;
    let Some(origin) = running_origin(state_dir) else { return logic::LiveCloudActionResult::NotRunning };
    let issued = (|| -> Result<(crate::auth::AuthService, String, String), String> {
        let store = crate::persistence::Store::open(state_dir.join("rust-state.sqlite")).map_err(|error| error.to_string())?;
        let secret: [u8; 32] = std::fs::read(state_dir.join("rust-signing-secret")).map_err(|error| error.to_string())?.try_into().map_err(|_| "Invalid native signing-secret length".to_owned())?;
        let auth = crate::auth::AuthService::new(store, secret, "t3_session_cli".into(), "loopback-browser".into()).map_err(|error| error.to_string())?;
        let (session, token) = auth
            .issue_session("cloud-cli", "bearer-access-token", vec![AuthEnvironmentScope::RelayWrite], serde_json::json!({"label":"t3 connect cli","deviceType":"bot"}), chrono::Utc::now(), chrono::Duration::minutes(5))
            .map_err(|error| error.to_string())?;
        Ok((auth, session.session_id, token))
    })();
    let (auth, session_id, token) = match issued {
        Ok(issued) => issued,
        Err(error) => return logic::LiveCloudActionResult::Failed(error),
    };
    let result = logic::run_live_cloud_unlink(Some(&origin), &token).await;
    if let Err(error) = auth.revoke(&session_id, chrono::Utc::now()) {
        tracing::warn!(%error, "could not revoke the session used to unlink");
    }
    result
}

fn environment_id(state_dir: &Path) -> Option<String> {
    let path = state_dir.join("rust-environment-id");
    std::fs::read_to_string(path).ok().map(|id| id.trim().to_owned()).filter(|id| !id.is_empty())
}

/// `disconnectCloud`.
async fn disconnect(context: &Context<'_>, clear_authorization: bool) -> Result<(), String> {
    let io = context.io;
    set_cli_desired_cloud_link(context.secrets.as_ref(), false, DesiredLinkMode::Managed).map_err(|error| error.message)?;
    let live = live_unlink(context).await;
    let relay = async {
        let token = match context.tokens.get_existing().await {
            Ok(token) => token,
            Err(_) => return Err(RelayRequestError::unexpected_response()),
        };
        if token.is_none() {
            return Ok(logic::RelayUnlinkResult::NotAuthenticated);
        }
        let env = |name: &str| (io.env)(name);
        let relay_url = match &io.relay_url {
            Some(url) => url.clone(),
            None => relay_url_config(&env, &BuildDefaults::default().relay_url).map_err(|_| RelayRequestError::unexpected_response())?,
        };
        let environment = environment_id(&io.state_dir).ok_or_else(RelayRequestError::unexpected_response)?;
        logic::unlink_relay_environment(io.control_plane.as_ref(), token.as_ref(), &relay_url, &environment).await
    }
    .await;
    clear_persisted_cloud_link(context.secrets.as_ref()).map_err(|error| error.message)?;
    if clear_authorization {
        context.tokens.clear().await.map_err(|error: CliTokenError| error.to_string())?;
    }
    for line in logic::report_cloud_disconnect_results(clear_authorization, &live, &relay) {
        if line.starts_with("T3 Connect is disabled, but") || line.starts_with("Could not revoke") {
            (io.warn)(&line);
        } else {
            (io.out)(&line);
        }
    }
    if clear_authorization {
        (io.out)(logic::LOGOUT_NOTICE);
    }
    Ok(())
}
