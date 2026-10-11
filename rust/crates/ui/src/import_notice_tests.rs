//! The first-run notice of the one-time import, through the mounted UI and the real native server: a
//! report beside the store becomes the notice, a connection that may not change settings cannot dismiss it, and
//! acknowledging it clears it for good.
use super::diff_tests::{Fixture, rendered_text, seed, until, wait_for_rendered_text};
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::transport_tests::{click_control, control, control_attribute};

const REPORT: &str = r#"{
  "importedAt": "2026-10-10T12:00:00Z", "sourceKind": "v2", "sourceMigration": 59, "sourceBytes": 4096,
  "projectsImported": 3, "threadsImported": 12, "events": {"thread.created": [12, 12]},
  "tables": {"provider_session_runtime": {"read": 4, "imported": 0, "disposition": "not carried: sessions end with the original"}},
  "warnings": ["One thread's provider session could not be resumed."],
  "credentialsToReenter": ["Sign in to Codex again."]
}"#;

#[tokio::test(flavor = "current_thread")]
async fn the_import_notice_lists_what_came_across_and_stays_until_acknowledged() {
    let fixture = Fixture::start(true).await;
    std::fs::write(fixture.store.state_dir().expect("a state directory").join("rust-import-report.json"), REPORT).unwrap();
    let all = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::SettingsWrite];
    let (mut dom, props) = fixture.client(Gate::default(), &all);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    wait_for_rendered_text(&mut dom, "Your T3 Code data was imported").await;
    let text = rendered_text(&dom);
    for expected in ["3 projects and 12 threads", "provider_session_runtime", "One thread's provider session could not be resumed.", "Sign in to Codex again."] {
        assert!(text.contains(expected), "{expected}: {text}");
    }
    click_control(&mut dom, "Got it");
    until(&mut dom, "the notice gone", |dom| control(dom, "Got it").is_none()).await;
    assert!(fixture.store.state_dir().unwrap().join("rust-import-acknowledged").exists(), "the server recorded that it was seen");
    assert_eq!(state.peek().error, None);
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_connection_that_cannot_change_settings_sees_the_notice_without_being_able_to_dismiss_it() {
    let fixture = Fixture::start(true).await;
    std::fs::write(fixture.store.state_dir().unwrap().join("rust-import-report.json"), REPORT).unwrap();
    let read_only = [AuthEnvironmentScope::OrchestrationRead];
    let (mut dom, _props) = fixture.client(Gate::default(), &read_only);
    wait_for_rendered_text(&mut dom, "Your T3 Code data was imported").await;
    assert_eq!(control_attribute(&dom, "Got it", "disabled").unwrap().1.as_deref(), Some("true"));
    fixture.finish(dom).await;
}

/// The app finds out that a server stopped on a failed import from what that server serves, and from nothing a working
/// server serves: the failed server's own router answers, and the working fixture's does not.
#[tokio::test(flavor = "current_thread")]
async fn a_server_that_stopped_on_a_failed_import_is_recognised_and_a_working_one_is_not() {
    let status = t3_server::import_status::failed("migration 61 is newer than this build reads (59)");
    let router = t3_server::import_status::failed_router(t3_server::import_status::FailedMode { environment: serde_json::json!({}), assets: None, status: status.clone() });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let found = super::probe_state_import(&address).await.expect("the failed server's status");
    assert_eq!(found.status, t3_contracts::StateImportOutcome::Failed);
    assert_eq!(found.error.as_deref(), Some("migration 61 is newer than this build reads (59)"));
    server.abort();

    let fixture = Fixture::start(false).await;
    assert_eq!(super::probe_state_import(&fixture.address).await, None);
    fixture.finish(rendered_nothing()).await;
}
fn rendered_nothing() -> VirtualDom {
    VirtualDom::new(|| rsx! {})
}
