//! "T3 Connect" through the mounted UI and the real native server, with T3 Connect's own services over the fakes of `t3-remote`'s
//! tests (a relay on loopback, a resolver for the relay client, a connector that never runs): what an unlinked and a linked
//! environment show, each switch and its failures, what a connection without the relay permissions can do, and the relay
//! client's installation step by step. The hosted service is never contacted.
use super::diff_tests::{Fixture, rendered_text, until, wait_for_rendered_text};
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::transport_tests::{change_control, click_control, click_when_enabled, control, control_attribute};
use std::sync::{Arc, Mutex};
use t3_remote::{
    relay_client::{ExecutableSource, InstallError, InstallFailureReason, InstallProgressStage, RelayClientResolver, RelayClientStatus},
    secrets::{CLOUD_ENDPOINT_RUNTIME_CONFIG, HOLD_WEBHOOKS_WHILE_OFFLINE_SECRET, PUBLISH_AGENT_ACTIVITY_SECRET, SecretStore},
};

use t3_remote::test_support as support;

const RELAY: &[AuthEnvironmentScope] = &[AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::RelayRead, AuthEnvironmentScope::RelayWrite];

/// A resolver that reports stages, waits at `Downloading` for the test, and then either installs or fails.
struct Resolver {
    status: Mutex<RelayClientStatus>,
    gate: tokio::sync::Semaphore,
    failure: Mutex<Option<InstallError>>,
}
impl Resolver {
    fn new(status: RelayClientStatus) -> Arc<Self> {
        Arc::new(Self { status: Mutex::new(status), gate: tokio::sync::Semaphore::new(0), failure: Mutex::new(None) })
    }
    fn available() -> RelayClientStatus {
        RelayClientStatus::Available { executable_path: "/fake/cloudflared".into(), source: ExecutableSource::Managed, version: "2025.1.0".into() }
    }
    fn missing() -> RelayClientStatus {
        RelayClientStatus::Missing { version: "2025.1.0".into() }
    }
}
#[async_trait::async_trait]
impl RelayClientResolver for Resolver {
    async fn resolve(&self) -> RelayClientStatus {
        self.status.lock().unwrap().clone()
    }
    async fn install_with_progress(&self, report: &(dyn Fn(InstallProgressStage) + Send + Sync)) -> Result<RelayClientStatus, InstallError> {
        report(InstallProgressStage::Checking);
        report(InstallProgressStage::Downloading);
        self.gate.acquire().await.unwrap().forget();
        if let Some(error) = self.failure.lock().unwrap().take() {
            return Err(error);
        }
        *self.status.lock().unwrap() = Self::available();
        Ok(self.status.lock().unwrap().clone())
    }
}

struct World {
    connect: Arc<t3_server::connect::Connect>,
    relay: support::FakeRelay,
    resolver: Arc<Resolver>,
}
impl World {
    fn secret(&self, name: &str) -> Option<String> {
        self.connect.secrets.get(name).unwrap().map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    }
}

/// T3 Connect over the fixture's store and sessions, as `main.rs` wires it, linked (with a managed tunnel) or not.
async fn install(fixture: &Fixture, linked: bool, managed: bool, status: RelayClientStatus, relay_refuses_preferences: bool) -> World {
    let relay = support::FakeRelay::start(move |request| {
        if request.path.ends_with("/link-preferences") && !relay_refuses_preferences {
            (200, request.body.clone())
        } else if request.path.ends_with("/link-preferences") {
            (500, serde_json::json!({}))
        } else {
            (404, serde_json::json!({}))
        }
    })
    .await;
    let resolver = Resolver::new(status);
    let keys = t3_remote::jwt::KeyPair::generate();
    let environment = serde_json::json!({"environmentId":"diff-ui","label":"Fixture","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}});
    let base = fixture.directory.path().join("connect-home");
    let secrets_dir = base.join("secrets");
    std::fs::create_dir_all(&secrets_dir).unwrap();
    let connect = t3_server::connect::Connect::new(t3_server::connect::ConnectOptions {
        store: fixture.store.clone(),
        auth: fixture.auth(),
        environment,
        base_dir: base,
        secrets_dir,
        relay_url: Some(relay.url.clone()),
        has_cloud_public_config: true,
        cli_oauth: t3_remote::config::CloudCliOAuthConfig {
            token_endpoint: String::new(),
            device_authorization_endpoint: String::new(),
            client_id: String::new(),
            loopback_port: 0,
            redirect_uri: String::new(),
            scopes: ["openid", "profile", "email", "offline_access"],
        },
        hosted_app_url: "https://app.t3.codes".into(),
        local_port: Arc::new(|| Some(1)),
        control_plane: Arc::new(t3_remote::control_plane::HttpControlPlane::new()),
        relay_client: resolver.clone(),
        connector_spawner: support::FakeSpawner::new(),
        clock: support::FixedClock::at(support::NOW_MS),
        random: Arc::new(t3_remote::backoff::FixedRandom(0.0)),
        allow_loopback_http_relay: true,
    })
    .unwrap();
    if linked {
        for (name, value) in support::linked_secrets(&relay.url, &keys) {
            connect.secrets.set(name, value.as_bytes()).unwrap();
        }
        if managed {
            connect.secrets.set(CLOUD_ENDPOINT_RUNTIME_CONFIG, support::config_json("connector-1", "tunnel-1").as_bytes()).unwrap();
        }
    }
    t3_server::connect::install(&fixture.store, connect.clone());
    World { connect, relay, resolver }
}

async fn page(fixture: &Fixture, scopes: &[AuthEnvironmentScope]) -> (VirtualDom, Store<UiModel>) {
    let (mut dom, props) = fixture.client(Gate::default(), scopes);
    let state = props.state.borrow().unwrap();
    until(&mut dom, "the shell", |_| state.peek().shell.synchronized).await;
    state.view().set(View::Connections);
    (dom, state)
}

fn disabled(dom: &VirtualDom, label: &str) -> bool {
    control_attribute(dom, label, "disabled").unwrap_or_else(|| panic!("no control {label}: {}", rendered_text(dom))).1.as_deref() == Some("true")
}
fn checked(dom: &VirtualDom, label: &str) -> bool {
    control_attribute(dom, label, "checked").unwrap_or_else(|| panic!("no control {label}")).1.as_deref() == Some("true")
}
fn title(dom: &VirtualDom, label: &str) -> Option<String> {
    control_attribute(dom, label, "title").and_then(|(_, value)| value)
}

#[tokio::test(flavor = "current_thread")]
async fn an_environment_nobody_linked_says_so_and_offers_only_what_needs_no_sign_in() {
    let fixture = Fixture::start(false).await;
    let world = install(&fixture, false, false, Resolver::available(), false).await;
    let (mut dom, _state) = page(&fixture, RELAY).await;
    wait_for_rendered_text(&mut dom, "Not linked").await;
    wait_for_rendered_text(&mut dom, "The relay client is installed.").await;
    // Linking needs the account sign-in this app cannot do: the switches say why they are unavailable.
    for label in ["Enable T3 Connect", "Publish agent activity to mobile clients"] {
        assert!(disabled(&dom, label), "{label}");
        assert_eq!(title(&dom, label).as_deref(), Some(t3_client::connect::SIGN_IN_REASON), "{label}");
        assert!(!checked(&dom, label), "{label}");
    }
    // Holding webhooks is a choice for a managed tunnel.
    assert!(control(&dom, "Hold webhook requests while this environment is offline").is_none());
    assert!(rendered_text(&dom).contains("Link it with `t3 connect`"));
    assert!(control(&dom, "Install relay client").is_none(), "an installed client is not offered again");
    assert!(world.relay.requests.lock().unwrap().is_empty(), "nothing was asked of the relay");
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_linked_environment_shows_its_link_and_each_switch_changes_it_on_the_server_and_in_the_relay() {
    let fixture = Fixture::start(false).await;
    let world = install(&fixture, true, true, Resolver::available(), false).await;
    let (mut dom, state) = page(&fixture, RELAY).await;
    wait_for_rendered_text(&mut dom, "Linked to account user-123").await;
    assert!(rendered_text(&dom).contains(&format!("Relay: {}", world.relay.url)));
    assert!(checked(&dom, "Enable T3 Connect") && !checked(&dom, "Publish agent activity to mobile clients"));
    assert!(!checked(&dom, "Hold webhook requests while this environment is offline"));

    // Publishing is a preference of the link.
    click_when_enabled_change(&mut dom, "Publish agent activity to mobile clients", "true").await;
    wait_for_rendered_text(&mut dom, "Agent activity enabled").await;
    until(&mut dom, "the switch on", |dom| checked(dom, "Publish agent activity to mobile clients")).await;
    assert_eq!(world.secret(PUBLISH_AGENT_ACTIVITY_SECRET).as_deref(), Some("true"));

    // Holding webhooks is the relay's to keep: the relay is asked, and the switch follows what the server then holds.
    click_when_enabled_change(&mut dom, "Hold webhook requests while this environment is offline", "true").await;
    wait_for_rendered_text(&mut dom, "Webhooks held while offline").await;
    until(&mut dom, "the switch on", |dom| checked(dom, "Hold webhook requests while this environment is offline")).await;
    assert_eq!(world.secret(HOLD_WEBHOOKS_WHILE_OFFLINE_SECRET).as_deref(), Some("true"));
    assert!(world.relay.requests.lock().unwrap().iter().any(|request| request.path.ends_with("/link-preferences") && request.body["holdWebhooksWhileOffline"] == true));

    // Dropping the tunnel while publishing would be a new, publish-only link: that needs the sign-in.
    assert!(disabled(&dom, "Enable T3 Connect"));
    assert_eq!(title(&dom, "Enable T3 Connect").as_deref(), Some(t3_client::connect::SIGN_IN_REASON));
    // With nothing published, turning the tunnel off is an unlink, which needs only this server.
    click_when_enabled_change(&mut dom, "Publish agent activity to mobile clients", "false").await;
    wait_for_rendered_text(&mut dom, "Agent activity disabled").await;
    until(&mut dom, "the switch off", |dom| !checked(dom, "Publish agent activity to mobile clients")).await;
    click_when_enabled_change(&mut dom, "Enable T3 Connect", "false").await;
    wait_for_rendered_text(&mut dom, "T3 Connect unlinked").await;
    wait_for_rendered_text(&mut dom, "Not linked").await;
    assert!(world.secret(CLOUD_ENDPOINT_RUNTIME_CONFIG).is_none() && world.secret(HOLD_WEBHOOKS_WHILE_OFFLINE_SECRET).is_none());
    assert!(control(&dom, "Hold webhook requests while this environment is offline").is_none());
    assert_eq!(state.error().peek().clone(), None);
    // The settings search finds the three entries on this page.
    for (query, id) in [("t3 connect", "t3-connect"), ("hold webhooks", "hold-webhooks-while-offline"), ("publish agent activity", "publish-agent-activity")] {
        assert!(crate::settings_common::find_settings(state, query).iter().any(|item| item.id == id), "{query}");
    }
    fixture.finish(dom).await;
}

async fn click_when_enabled_change(dom: &mut VirtualDom, label: &str, value: &str) {
    crate::runtime::transport_tests::until_enabled(dom, label).await;
    change_control(dom, label, value);
}

#[tokio::test(flavor = "current_thread")]
async fn a_preference_the_relay_refuses_is_reported_with_the_servers_words_and_not_kept() {
    let fixture = Fixture::start(false).await;
    let world = install(&fixture, true, true, Resolver::available(), true).await;
    let (mut dom, _state) = page(&fixture, RELAY).await;
    wait_for_rendered_text(&mut dom, "Linked to account user-123").await;
    click_when_enabled_change(&mut dom, "Hold webhook requests while this environment is offline", "true").await;
    wait_for_rendered_text(&mut dom, "Could not update environment cloud preferences: ").await;
    wait_for_rendered_text(&mut dom, "Could not update T3 Connect").await;
    // The state is read again after the failure: the switch is what the server holds.
    until(&mut dom, "the switch reverted and usable", |dom| !checked(dom, "Hold webhook requests while this environment is offline") && !disabled(dom, "Hold webhook requests while this environment is offline")).await;
    assert!(world.secret(HOLD_WEBHOOKS_WHILE_OFFLINE_SECRET).is_none());
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn without_the_manage_permission_the_switches_are_unavailable_and_without_either_there_is_nothing_or_a_pointer() {
    let fixture = Fixture::start(false).await;
    let _world = install(&fixture, true, true, Resolver::available(), false).await;
    let (mut dom, _state) = page(&fixture, &[AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::RelayRead]).await;
    wait_for_rendered_text(&mut dom, "Linked to account user-123").await;
    for label in ["Enable T3 Connect", "Publish agent activity to mobile clients", "Hold webhook requests while this environment is offline"] {
        assert!(disabled(&dom, label), "{label}");
        assert_eq!(title(&dom, label).as_deref(), Some(t3_client::connect::PERMISSION_REASON), "{label}");
    }
    fixture.finish(dom).await;

    let fixture = Fixture::start(false).await;
    let _world = install(&fixture, true, true, Resolver::available(), false).await;
    let (mut dom, _state) = page(&fixture, &[AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::RelayWrite]).await;
    wait_for_rendered_text(&mut dom, t3_client::connect::NO_READ_NOTICE).await;
    assert!(control(&dom, "Publish agent activity to mobile clients").is_none());
    fixture.finish(dom).await;

    let fixture = Fixture::start(false).await;
    let _world = install(&fixture, true, true, Resolver::available(), false).await;
    let (mut dom, _state) = page(&fixture, &[AuthEnvironmentScope::OrchestrationRead]).await;
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    assert!(!rendered_text(&dom).contains("T3 Connect"));
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn the_relay_client_is_installed_after_a_confirmation_with_its_steps_shown_and_a_failure_is_reported() {
    let fixture = Fixture::start(false).await;
    let world = install(&fixture, false, false, Resolver::missing(), false).await;
    let (mut dom, _state) = page(&fixture, RELAY).await;
    wait_for_rendered_text(&mut dom, "The relay client 2025.1.0 is not installed.").await;
    // Declining changes nothing.
    click_when_enabled(&mut dom, "Install relay client").await;
    wait_for_rendered_text(&mut dom, "T3 Code will download and install version 2025.1.0 locally.").await;
    click_control(&mut dom, "Cancel installing the relay client");
    until(&mut dom, "the dialog gone", |dom| control(dom, "Download and install").is_none()).await;
    assert!(control(&dom, "Install relay client").is_some());

    // A failure ends the dialog and says why.
    *world.resolver.failure.lock().unwrap() = Some(InstallError { reason: InstallFailureReason::DownloadFailed, message: "The download failed.".into() });
    click_control(&mut dom, "Install relay client");
    click_when_enabled(&mut dom, "Download and install").await;
    wait_for_rendered_text(&mut dom, "Downloading relay client").await;
    assert!(rendered_text(&dom).contains("3 of 7"));
    world.resolver.gate.add_permits(1);
    wait_for_rendered_text(&mut dom, "Could not install the relay client").await;
    wait_for_rendered_text(&mut dom, "The download failed.").await;
    until(&mut dom, "the dialog gone", |dom| control(dom, "Installing relay client").is_none()).await;
    assert!(control(&dom, "Install relay client").is_some());

    // Then it installs, and the row follows.
    click_control(&mut dom, "Install relay client");
    click_when_enabled(&mut dom, "Download and install").await;
    wait_for_rendered_text(&mut dom, "Downloading relay client").await;
    world.resolver.gate.add_permits(1);
    wait_for_rendered_text(&mut dom, "Relay client installed").await;
    wait_for_rendered_text(&mut dom, "The relay client is installed.").await;
    assert!(control(&dom, "Install relay client").is_none());

    // Without the manage permission the install is not offered to start.
    fixture.finish(dom).await;
    let fixture = Fixture::start(false).await;
    let _world = install(&fixture, false, false, Resolver::missing(), false).await;
    let (mut dom, _state) = page(&fixture, &[AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::RelayRead]).await;
    wait_for_rendered_text(&mut dom, "The relay client 2025.1.0 is not installed.").await;
    assert!(disabled(&dom, "Install relay client"));
    fixture.finish(dom).await;
}
