//! Updating the connected server from the client, through the mounted UI and the real native server: the banner when the
//! server is behind, the command for a server that cannot update itself, the refusal of a server with no way to update, and a
//! desktop-managed server driven by a fake desktop app (the server's own seam for it) through the confirmation, the progress
//! it reports and the failure its commit ends in. Waits are on the banner's own text.
use super::diff_tests::{Fixture, open_thread, rendered_text, seed, until, wait_for_rendered_text};
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::transport_tests::{click_control, control};
use async_trait::async_trait;
use t3_contracts::ServerSelfUpdateMethod;
use std::{convert::Infallible, sync::{Arc, Mutex}};
use t3_remote::self_update::{DesktopAppUpdate, HandoffCallback, ProgressReporter, ServerSelfUpdateError, ServerSelfUpdateProgressStage, ServerSelfUpdateResult};

const MAINTAINER: [AuthEnvironmentScope; 3] = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::EnvironmentMaintain];

/// A desktop app that downloads on request, waits to be let go, hands back a token, and refuses to relaunch.
struct FakeDesktop {
    release: Arc<tokio::sync::Notify>,
    commits: Arc<Mutex<Vec<String>>>,
}
#[async_trait]
impl DesktopAppUpdate for FakeDesktop {
    fn available(&self) -> bool {
        true
    }
    async fn run(&self, report: ProgressReporter) -> Result<ServerSelfUpdateResult, ServerSelfUpdateError> {
        report(ServerSelfUpdateProgressStage::Downloading).await?;
        self.release.notified().await;
        report(ServerSelfUpdateProgressStage::Installing).await?;
        Ok(ServerSelfUpdateResult { target_version: "0.1.0".into(), method: ServerSelfUpdateMethod::DesktopApp, update_id: None, desktop_update_token: Some("prepared-token".into()) })
    }
    async fn commit(&self, request_id: &str, _accepted: HandoffCallback) -> Result<Infallible, ServerSelfUpdateError> {
        self.commits.lock().unwrap().push(request_id.to_owned());
        Err(ServerSelfUpdateError::new("The desktop app refused to relaunch."))
    }
}

fn banner(dom: &VirtualDom) -> String {
    rendered_text(dom)
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_server_behind_its_client_offers_the_command_when_it_cannot_update_itself_and_the_notice_stays_dismissed() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.client(Gate::default(), &MAINTAINER);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the configuration", |_| state.peek().config["environment"]["serverVersion"].is_string()).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    // The fixture's server calls itself "rust-test", which is not this client's version: it is behind.
    wait_for_rendered_text(&mut dom, "Server update available").await;
    until(&mut dom, "the command", |dom| control(dom, "Copy relaunch command for Isolated integration").is_some()).await;
    assert!(control(&dom, "Update Isolated integration").is_none(), "a server that cannot update itself gets no update button");
    // Dismissing hides it, and the version pair is remembered.
    click_control(&mut dom, "Dismiss update notice");
    until(&mut dom, "the notice gone", |dom| !banner(dom).contains("Server update available")).await;
    assert_eq!(state.peek().server_update.dismissed.len(), 1);
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_server_that_offers_an_update_but_has_no_way_to_run_it_leaves_its_refusal_in_the_banner() {
    let fixture = Fixture::start_offering(json!({"serverSelfUpdate": "boot-service"})).await;
    let (mut dom, props) = fixture.client(Gate::default(), &MAINTAINER);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the configuration", |_| state.peek().config["environment"]["serverVersion"].is_string()).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    wait_for_rendered_text(&mut dom, "Update to stay in sync").await;
    click_control(&mut dom, "Update Isolated integration");
    // This server was started with no launcher: it says so and downloads and installs nothing.
    wait_for_rendered_text(&mut dom, "Could not update Isolated integration").await;
    assert!(banner(&dom).contains("Remote updates require the T3 Code background service"), "{}", banner(&dom));
    assert!(control(&dom, "Retry Isolated integration").is_some());
    // Dismissing the failure leaves the notice off until another attempt.
    click_control(&mut dom, "Dismiss update notice");
    until(&mut dom, "the failure gone", |dom| !banner(dom).contains("Could not update")).await;
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_desktop_managed_server_asks_first_shows_the_apps_progress_and_the_failure_of_its_relaunch() {
    let fixture = Fixture::start_offering(json!({"serverSelfUpdate": "desktop-managed", "desktopAppUpdate": true, "serverSelfUpdateProgress": true})).await;
    let release = Arc::new(tokio::sync::Notify::new());
    let commits = Arc::new(Mutex::new(Vec::new()));
    let service = t3_server::self_update_rpc::build(true, Arc::new(FakeDesktop { release: release.clone(), commits: commits.clone() }), fixture.directory.path().join("update"), String::new(), &Default::default()).expect("the update service");
    t3_server::self_update_rpc::install(&fixture.store, service);
    let (mut dom, props) = fixture.client(Gate::default(), &MAINTAINER);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the configuration", |_| state.peek().config["environment"]["serverVersion"].is_string()).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    until(&mut dom, "the update button", |dom| control(dom, "Update Isolated integration").is_some()).await;

    // The app closes and relaunches on that machine, so the person is asked first; declining sends nothing.
    click_control(&mut dom, "Update Isolated integration");
    wait_for_rendered_text(&mut dom, "Update the T3 Code desktop app that runs the Isolated integration?").await;
    click_control(&mut dom, "Cancel update");
    until(&mut dom, "the question gone", |dom| !banner(dom).contains("desktop app that runs")).await;
    assert!(commits.lock().unwrap().is_empty());
    assert!(matches!(state.peek().server_update.state, t3_client::server_update::State::Idle));

    // Confirmed, the app downloads: the banner says so while it does, and nothing else can start.
    click_control(&mut dom, "Update Isolated integration");
    click_control(&mut dom, "Confirm");
    wait_for_rendered_text(&mut dom, "Updating Isolated integration").await;
    wait_for_rendered_text(&mut dom, "Downloading\u{2026}").await;
    assert!(control(&dom, "Update Isolated integration").is_none(), "no second update while one runs");
    release.notify_one();

    // The app prepared the update; committing it is what relaunches the app, and this one refuses.
    wait_for_rendered_text(&mut dom, "Could not update Isolated integration").await;
    assert!(banner(&dom).contains("The desktop app refused to relaunch."), "{}", banner(&dom));
    assert_eq!(commits.lock().unwrap().as_slice(), ["prepared-token"]);
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_connection_that_may_not_maintain_the_server_cannot_start_an_update() {
    let fixture = Fixture::start_offering(json!({"serverSelfUpdate": "boot-service"})).await;
    let (mut dom, props) = fixture.client(Gate::default(), &[AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate]);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the configuration", |_| state.peek().config["environment"]["serverVersion"].is_string()).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    until(&mut dom, "the update button", |dom| control(dom, "Update Isolated integration").is_some()).await;
    let disabled = crate::runtime::transport_tests::control_attribute(&dom, "Update Isolated integration", "disabled").and_then(|(_, value)| value);
    assert_eq!(disabled.as_deref(), Some("true"));
    fixture.finish(dom).await;
}
