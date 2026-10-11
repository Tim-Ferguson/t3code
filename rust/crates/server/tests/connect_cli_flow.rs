//! `t3-server connect ...` against a state directory: what each subcommand prints and stores, with a Clerk (the sign-in service) and
//! a relay on loopback, and the running server of `connect_support` for the part of `unlink` that asks it to stop its tunnel.
mod connect_support;

use async_trait::async_trait;
use base64::Engine;
use connect_support::*;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use t3_remote::{
    cli_token::{LoginAction, LoginInteraction},
    config::CloudCliOAuthConfig,
    relay_client::RelayClientStatus,
    secrets::*,
};
use t3_server::{
    connect::ConnectSecrets,
    connect_cli::{CliEnvironment, Flags, Subcommand, run},
    server_secret_store::ServerSecretStore,
};

struct NoTerminal;

#[async_trait]
impl LoginInteraction for NoTerminal {
    fn show_authorization_url(&self, _: &str) {}
    async fn next_action(&self) -> LoginAction {
        std::future::pending().await
    }
    async fn launch_browser(&self, _: &str) -> Result<(), String> {
        Err("no browser".into())
    }
    fn warn(&self, _: &str) {}
}

struct Cli {
    environment: CliEnvironment,
    out: Arc<Mutex<Vec<String>>>,
    warned: Arc<Mutex<Vec<String>>>,
    resolver: Arc<FixedResolver>,
    declined: Arc<Mutex<Vec<String>>>,
    accept_install: Arc<std::sync::atomic::AtomicBool>,
    secrets: ConnectSecrets,
}

fn id_token(email: &str) -> String {
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json!({"email":email,"sub":"user_1"}).to_string());
    format!("e30.{payload}.sig")
}

/// A sign-in service that approves the device at once.
async fn clerk() -> FakeRelay {
    FakeRelay::start_raw_form(|path, fields| match path {
        "/oauth/device_authorization" => {
            assert_eq!(fields["client_id"], "oauth_client");
            (200, json!({"device_code":"device-1","user_code":"BCDF-GHJK","verification_uri":"https://accounts.example.test/device","expires_in":600,"interval":0}))
        }
        "/oauth/token" => (200, json!({"access_token":"cli-access","refresh_token":"cli-refresh","expires_in":3600,"token_type":"bearer","id_token":id_token("me@example.test")})),
        _ => (404, json!({})),
    })
    .await
}

fn cli(state_dir: &std::path::Path, relay_url: Option<&str>, clerk_url: Option<&str>) -> Cli {
    let out: Arc<Mutex<Vec<String>>> = Default::default();
    let warned: Arc<Mutex<Vec<String>>> = Default::default();
    let declined: Arc<Mutex<Vec<String>>> = Default::default();
    let accept_install = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let resolver = FixedResolver::available();
    let (o, w, d, a) = (out.clone(), warned.clone(), declined.clone(), accept_install.clone());
    let environment = CliEnvironment {
        state_dir: state_dir.to_owned(),
        env: Arc::new(|_| None),
        interaction: Arc::new(NoTerminal),
        confirm_install: Arc::new(move |version| {
            d.lock().unwrap().push(version.to_owned());
            a.load(std::sync::atomic::Ordering::SeqCst)
        }),
        out: Arc::new(move |text| o.lock().unwrap().push(text.to_owned())),
        warn: Arc::new(move |text| w.lock().unwrap().push(text.to_owned())),
        control_plane: Arc::new(t3_remote::control_plane::HttpControlPlane::new()),
        relay_client: resolver.clone(),
        clock: FixedClock::at(chrono::Utc::now().timestamp_millis()),
        oauth: clerk_url.map(|base| CloudCliOAuthConfig {
            token_endpoint: format!("{base}/oauth/token"),
            device_authorization_endpoint: format!("{base}/oauth/device_authorization"),
            client_id: "oauth_client".into(),
            loopback_port: 0,
            redirect_uri: "http://127.0.0.1:0/callback".into(),
            scopes: ["openid", "profile", "email", "offline_access"],
        }),
        relay_url: relay_url.map(str::to_owned),
    };
    Cli { environment, out, warned, resolver, declined, accept_install, secrets: ConnectSecrets(ServerSecretStore::open(state_dir.join("secrets")).unwrap()) }
}

impl Cli {
    async fn run(&self, subcommand: Subcommand, flags: Flags) -> Result<String, String> {
        self.out.lock().unwrap().clear();
        self.warned.lock().unwrap().clear();
        run(subcommand, flags, &self.environment).await?;
        Ok(self.out.lock().unwrap().join("\n"))
    }
    fn secret(&self, name: &str) -> Option<String> {
        self.secrets.get(name).unwrap().map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    }
}

fn state_dir() -> (tempfile::TempDir, std::path::PathBuf) {
    let directory = tempfile::Builder::new().prefix("t3code-rust-port-test-connect-cli-").tempdir().unwrap();
    let state_dir = directory.path().join("userdata");
    std::fs::create_dir_all(&state_dir).unwrap();
    std::fs::write(state_dir.join("rust-environment-id"), "env_123").unwrap();
    (directory, state_dir)
}

const NONE: Flags = Flags { headless: false, publish_only: false, disable: false, json: false };

#[tokio::test]
async fn login_signs_in_with_the_device_flow_and_status_shows_what_is_saved() {
    let (_directory, state_dir) = state_dir();
    let clerk = clerk().await;
    let cli = cli(&state_dir, None, Some(&clerk.url));
    let before = cli.run(Subcommand::Status, NONE).await.unwrap();
    assert!(before.contains("  Exposure: disabled") && before.contains("  Authorization: missing") && before.contains("  Relay: not provisioned"), "{before}");
    assert!(before.contains("Next: Run `t3 connect link` to authorize and enable T3 Connect."), "{before}");
    let json_before: Value = serde_json::from_str(&cli.run(Subcommand::Status, Flags { json: true, ..NONE }).await.unwrap()).unwrap();
    assert_eq!((json_before["desired"].as_bool(), json_before["authenticated"].as_bool(), json_before["linked"].as_bool(), json_before["relayClient"]["status"].as_str()), (Some(false), Some(false), Some(false), Some("available")));

    let login = cli.run(Subcommand::Login, Flags { headless: true, ..NONE }).await.unwrap();
    assert!(login.starts_with("T3 Connect\n"), "{login}");
    assert!(login.contains("Headless authorization") && login.contains("Confirm this code when asked: BCDF-GHJK") && login.contains("https://accounts.example.test/device"), "{login}");
    assert!(login.ends_with("✓ Signed in as me@example.test"), "{login}");
    let stored: Value = serde_json::from_str(&cli.secret(CLOUD_CLI_OAUTH_TOKEN_SECRET).unwrap()).unwrap();
    assert_eq!((stored["accessToken"].as_str(), stored["identity"].as_str()), (Some("cli-access"), Some("me@example.test")));
    let after = cli.run(Subcommand::Status, NONE).await.unwrap();
    assert!(after.contains("  Authorization: stored credential") && after.contains("Next: Run `t3 connect link` to enable T3 Connect."), "{after}");
}

#[tokio::test]
async fn link_installs_the_relay_client_if_wanted_signs_in_and_records_the_wish() {
    let (_directory, state_dir) = state_dir();
    let clerk = clerk().await;
    let cli = cli(&state_dir, None, Some(&clerk.url));
    // A missing relay client is installed only after the person says so; declining stops before any sign-in.
    cli.resolver.set(RelayClientStatus::Missing { version: "2026.5.2".into() });
    let declined = cli.run(Subcommand::Link, NONE).await.unwrap();
    assert!(declined.ends_with("T3 Connect setup cancelled. The relay client was not installed."), "{declined}");
    assert_eq!(*cli.declined.lock().unwrap(), ["2026.5.2"]);
    assert!(cli.secret(CLOUD_CLI_OAUTH_TOKEN_SECRET).is_none() && cli.secret(CLOUD_CLI_DESIRED_LINK_SECRET).is_none());
    // Accepting installs it (the fake reports one stage) and goes on to sign in. `--headless` takes the device flow.
    cli.accept_install.store(true, std::sync::atomic::Ordering::SeqCst);
    let linked = cli.run(Subcommand::Link, Flags { headless: true, ..NONE }).await.unwrap();
    assert!(linked.contains("Relay client: Checking existing installation..."), "{linked}");
    assert!(linked.contains("✓ Authorized as me@example.test\n\nNext\n  Start the server with `t3-server serve` to make this machine reachable."), "{linked}");
    assert_eq!(cli.secret(CLOUD_CLI_DESIRED_LINK_SECRET).as_deref(), Some("managed"));
    let status = cli.run(Subcommand::Status, NONE).await.unwrap();
    assert!(status.contains("  Exposure: enabled") && status.contains("Environment link: pending server startup") && status.contains("Next: Start T3 to provision"), "{status}");
}

#[tokio::test]
async fn a_publish_only_link_needs_no_relay_client_and_turns_publishing_on() {
    let (_directory, state_dir) = state_dir();
    let clerk = clerk().await;
    let cli = cli(&state_dir, None, Some(&clerk.url));
    cli.resolver.set(RelayClientStatus::Unsupported { platform: "plan9".into(), arch: "mips".into(), version: "2026.5.2".into() });
    let linked = cli.run(Subcommand::Link, Flags { headless: true, publish_only: true, ..NONE }).await.unwrap();
    assert!(!linked.contains("Relay client"), "{linked}");
    assert!(linked.contains("Next\n  Start T3 to publish agent activity (no managed tunnel)."), "{linked}");
    assert_eq!(cli.secret(CLOUD_CLI_DESIRED_LINK_SECRET).as_deref(), Some("publish_only"));
    assert_eq!(cli.secret(PUBLISH_AGENT_ACTIVITY_SECRET).as_deref(), Some("true"));
    // Disabling publishing also cancels a publish-only link that has not been made yet.
    let disabled = cli.run(Subcommand::Publish, Flags { disable: true, ..NONE }).await.unwrap();
    assert_eq!(disabled, "Cancelled the pending publish-only T3 Connect link.\nPublishing agent activity disabled.");
    assert_eq!(cli.secret(PUBLISH_AGENT_ACTIVITY_SECRET).as_deref(), Some("false"));
    assert_ne!(cli.secret(CLOUD_CLI_DESIRED_LINK_SECRET).as_deref(), Some("publish_only"));
    // Enabling it again asks for a restart, because the sign-in is stored and no link is pending.
    let enabled = cli.run(Subcommand::Publish, NONE).await.unwrap();
    assert_eq!(enabled, "Publishing agent activity enabled.\nRestart T3 to finish authorizing this environment to publish (no managed tunnel is created).");
}

#[tokio::test]
async fn publishing_without_a_sign_in_says_to_sign_in_first() {
    let (_directory, state_dir) = state_dir();
    let cli = cli(&state_dir, None, None);
    let enabled = cli.run(Subcommand::Publish, NONE).await.unwrap();
    assert_eq!(enabled, "Publishing agent activity enabled.\nRun `t3 connect login` first so this environment can be authorized to publish.");
    // The sign-in was never configured here, so a command that needs one gets the error, and nothing is requested.
    let error = cli.run(Subcommand::Login, Flags { headless: true, ..NONE }).await.unwrap_err();
    assert!(!error.is_empty());
}

#[tokio::test]
async fn unlink_and_logout_revoke_the_relay_record_and_forget_the_link() {
    let (_directory, state_dir) = state_dir();
    let relay = FakeRelay::start(|request| if request.method == "DELETE" { (200, json!({"ok":true})) } else { (404, json!({})) }).await;
    let cli = cli(&state_dir, Some(&relay.url), None);
    for (name, value) in [
        (CLOUD_LINKED_USER_ID, "user-123"),
        (RELAY_URL_SECRET, relay.url.as_str()),
        (CLOUD_CLI_DESIRED_LINK_SECRET, "managed"),
        (CLOUD_ENDPOINT_RUNTIME_CONFIG, r#"{"providerKind":"cloudflare_tunnel","connectorToken":"c"}"#),
        (PUBLISH_AGENT_ACTIVITY_SECRET, "true"),
        (HOLD_WEBHOOKS_WHILE_OFFLINE_SECRET, "true"),
    ] {
        cli.secrets.set(name, value.as_bytes()).unwrap();
    }
    let token = json!({"accessToken":"cli-access","refreshToken":"r","expiresAtEpochMs":chrono::Utc::now().timestamp_millis() + 3_600_000,"identity":"me@example.test"});
    cli.secrets.set(CLOUD_CLI_OAUTH_TOKEN_SECRET, token.to_string().as_bytes()).unwrap();

    // No server is running: it is disabled locally, and the relay's record of this environment is deleted.
    let unlinked = cli.run(Subcommand::Unlink, NONE).await.unwrap();
    assert_eq!(unlinked, "T3 Connect is disabled locally.\nRevoked the relay-side environment record.");
    let deleted = relay.recorded_for("DELETE", "/v1/client/environment-links/env_123");
    assert_eq!((deleted.len(), deleted[0].authorization.as_deref()), (1, Some("Bearer cli-access")));
    assert!(cli.secret(CLOUD_LINKED_USER_ID).is_none() && cli.secret(CLOUD_ENDPOINT_RUNTIME_CONFIG).is_none());
    assert_ne!(cli.secret(CLOUD_CLI_DESIRED_LINK_SECRET).as_deref(), Some("managed"), "the wish is withdrawn");
    assert!(cli.secret(CLOUD_CLI_OAUTH_TOKEN_SECRET).is_some(), "unlink keeps the sign-in");
    // The opt-in to hold webhooks is left behind, as the original leaves it; the one to publish goes with the link.
    // TODO(original bug, cloud/CliState.ts clearPersistedCloudLink): it clears the publishing opt-in but not this one.
    assert_eq!(cli.secret(HOLD_WEBHOOKS_WHILE_OFFLINE_SECRET).as_deref(), Some("true"));
    assert!(cli.secret(PUBLISH_AGENT_ACTIVITY_SECRET).is_none());

    let logout = cli.run(Subcommand::Logout, NONE).await.unwrap();
    assert!(logout.ends_with("Signed out of T3 Connect locally.\nThe background service is managed separately with `t3 service`."), "{logout}");
    assert!(cli.secret(CLOUD_CLI_OAUTH_TOKEN_SECRET).is_none());
    // Without a sign-in there is no relay record to revoke, and nothing is said about it.
    // Logging out revoked the record too (it still had the sign-in then); a third call has nothing to revoke it with.
    assert_eq!(relay.recorded_for("DELETE", "/v1/client/environment-links/env_123").len(), 2);
    assert_eq!(cli.run(Subcommand::Unlink, NONE).await.unwrap(), "T3 Connect is disabled locally.");
    assert_eq!(relay.recorded_for("DELETE", "/v1/client/environment-links/env_123").len(), 2);
}

#[tokio::test]
async fn unlink_asks_a_running_server_to_stop_its_tunnel_with_a_session_it_revokes_afterwards() {
    let relay_hits = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = relay_hits.clone();
    let server = start(Options { start: false, ..Options::new(move |request| {
        seen.lock().unwrap().push(format!("{} {}", request.method, request.path));
        if request.method == "DELETE" || request.path.ends_with("/tunnel/recovery") { (200, json!({"ok":true,"status":"ready"})) } else { (404, json!({})) }
    }) })
    .await;
    // The server's files the CLI reads: its identity, its signing secret and where it listens.
    std::fs::write(server.state_dir.join("rust-environment-id"), ENVIRONMENT_ID).unwrap();
    std::fs::write(server.state_dir.join("rust-signing-secret"), [7u8; 32]).unwrap();
    t3_server::connect::persist_runtime_state(&t3_server::connect_cli::runtime_state_path(&server.state_dir), "127.0.0.1", server.address.port());
    for (name, value) in connect_support::linked(&server.relay.url, &server.relay_keys) {
        server.connect.secrets.set(&name, value.as_bytes()).unwrap();
    }
    let config = t3_remote::types::RuntimeConfig::cloudflare("connector-1", Some("tunnel-1"));
    let confirmed = t3_remote::types::ConfirmedOrigin { config: config.clone(), origin: t3_remote::types::ManagedEndpointOrigin::new("127.0.0.1", server.address.port()) };
    server.connect.secrets.set(CLOUD_ENDPOINT_RUNTIME_CONFIG, serde_json::to_string(&config).unwrap().as_bytes()).unwrap();
    server.connect.secrets.set(CLOUD_ENDPOINT_CONFIRMED_ORIGIN, serde_json::to_string(&confirmed).unwrap().as_bytes()).unwrap();
    server.sign_in_cli();
    // The CLI reads the stored sign-in on the real clock.
    let token = json!({"accessToken":"cli-access","refreshToken":"r","expiresAtEpochMs":chrono::Utc::now().timestamp_millis() + 3_600_000});
    server.connect.secrets.set(CLOUD_CLI_OAUTH_TOKEN_SECRET, token.to_string().as_bytes()).unwrap();
    server.connect.start();
    server.spawner.wait_for_spawns(1).await;
    assert!(server.spawner.last().is_running());

    let cli = cli(&server.state_dir, Some(&server.relay.url), None);
    let unlinked = cli.run(Subcommand::Unlink, NONE).await.unwrap();
    // The running server stopped its connector itself, and the relay's record was revoked; no warning about the server.
    assert_eq!(unlinked, "T3 Connect is disabled locally.\nRevoked the relay-side environment record.");
    assert!(cli.warned.lock().unwrap().is_empty());
    assert!(!server.spawner.last().is_running());
    // The short-lived session the CLI used is gone.
    let sessions = server.auth.list_client_sessions("none", chrono::Utc::now()).unwrap();
    assert!(sessions.iter().all(|session| session.subject.as_str() != "cloud-cli"), "{sessions:?}");
    server.finish().await;
}
