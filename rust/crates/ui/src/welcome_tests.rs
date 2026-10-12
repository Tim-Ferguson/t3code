//! The first-run wizard, through the mounted UI and the real native server: it opens over the workspace, walks the three steps
//! with the computer that is connected and the agents the server reports, and finishing it records that onboarding is done.
use super::diff_tests::{Fixture, rendered_text, seed, until, wait_for_rendered_text};
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::transport_tests::{click_control, control};

/// A server that has no project and no thread, started fresh, sends the plain `welcome` it really sends (the working directory and no
/// bootstrap fields); the gate takes that as an empty workspace and opens the wizard by itself, as the original's does.
#[tokio::test(flavor = "current_thread")]
async fn a_fresh_install_opens_the_wizard_by_itself() {
    let fixture = Fixture::start(false).await;
    fixture.publish_startup_welcome();
    let (mut dom, props) = fixture.first_launch(Gate::default(), &[AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate]);
    let state = props.state.borrow().expect("the app is mounted");
    until(&mut dom, "the wizard", |_| state.peek().welcome.open).await;
    wait_for_rendered_text(&mut dom, "Connect your computers").await;
    let model = state.peek();
    assert!(model.server_welcome.is_some(), "the gate waited for the server's welcome");
    assert_eq!(model.server_welcome.as_ref().and_then(|welcome| welcome.bootstrap_project_created), None, "the real welcome carries no bootstrap flags");
    assert!(model.client_settings.onboarding_completed_at.is_none(), "an empty workspace is not recorded as done");
    drop(model);
    fixture.finish(dom).await;
}

/// A workspace that already has a project and threads is not a first run: the app opens and onboarding is recorded as done. The
/// projects exist before the app connects, as they do for a person who used the app before onboarding existed.
#[tokio::test(flavor = "current_thread")]
async fn a_workspace_with_projects_goes_straight_to_the_app() {
    let fixture = Fixture::start(false).await;
    {
        let (mut earlier, props) = fixture.operator(Gate::default());
        seed(&mut earlier, &props, &fixture.workspace).await;
    }
    fixture.publish_startup_welcome();
    let (mut dom, props) = fixture.first_launch(Gate::default(), &[AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate]);
    let state = props.state.borrow().expect("the app is mounted");
    until(&mut dom, "onboarding recorded", |_| state.peek().client_settings.onboarding_completed_at.is_some()).await;
    assert!(!state.peek().welcome.open);
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn the_wizard_walks_connect_agents_and_projects_and_skipping_the_import_finishes_it() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    assert!(state.peek().client_settings.onboarding_completed_at.is_none());

    dom.in_scope(ScopeId::APP, || crate::welcome::open(state));
    wait_for_rendered_text(&mut dom, "Connect your computers").await;
    let text = rendered_text(&dom);
    assert!(text.contains("1. Connect") && text.contains("2. Agents") && text.contains("3. Projects"), "{text}");
    // The computer this app is connected to is the one to set up.
    until(&mut dom, "the connection", |dom| rendered_text(dom).contains("Connected")).await;
    click_control(&mut dom, "Continue to agents");

    wait_for_rendered_text(&mut dom, "Connect your agents").await;
    let text = rendered_text(&dom);
    assert!(text.contains("Codex") && text.contains("Ready to code.") && text.contains("Claude") && text.contains("Disabled"), "both primary agents have a card: {}", text.split("Connect your agents").nth(1).unwrap_or_default());
    click_control(&mut dom, "Continue to projects");

    wait_for_rendered_text(&mut dom, "Choose your projects").await;
    until(&mut dom, "the scan to answer", |dom| control(dom, "Do not import projects").is_some()).await;
    click_control(&mut dom, "Do not import projects");

    // Finishing records it, closes the wizard and leaves the person on the start form.
    until(&mut dom, "the wizard to close", |_| !state.peek().welcome.open).await;
    assert!(state.peek().client_settings.onboarding_completed_at.is_some(), "the gate never opens it again");
    until(&mut dom, "the start form", |dom| control(dom, "First message").is_some()).await;
    assert!(!rendered_text(&dom).contains("Connect your agents"));
    fixture.finish(dom).await;
}
