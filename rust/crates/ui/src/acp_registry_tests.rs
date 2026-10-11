//! The ACP registry step of the add-provider dialog through the mounted UI and the real native server. The fixture server has no
//! registry catalog (a catalog reads the registry from the network, which a test must not), so the step shows the server's own
//! answer to the search; what a prepared agent becomes and the buttons it gets are the client logic's tests
//! (`t3_client::acp_registry`). Waits are on the text the step draws.
use super::diff_tests::{Fixture, rendered_text, seed, until, wait_for_rendered_text};
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::transport_tests::{change_control, click_control, control};

const MANAGER: [AuthEnvironmentScope; 4] = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::SettingsWrite, AuthEnvironmentScope::ProvidersManage];

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn the_registry_step_asks_the_server_and_shows_its_answer_and_leads_back_to_the_local_command() {
    // The server's catalog reads the registry from the network, which a test must not reach: every secure request goes to a
    // proxy address nothing listens on and fails at once.
    unsafe {
        std::env::set_var("HTTPS_PROXY", "http://127.0.0.1:9");
        std::env::set_var("https_proxy", "http://127.0.0.1:9");
        std::env::remove_var("NO_PROXY");
        std::env::remove_var("no_proxy");
    }
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.client(Gate::default(), &MANAGER);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the configuration", |_| state.peek().config["settings"].is_object()).await;
    state.view().set(View::Providers);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    until(&mut dom, "the toolbar", |dom| control(dom, "Add provider").is_some()).await;
    click_control(&mut dom, "Add provider");
    change_control(&mut dom, "Provider driver", "acp-registry");

    // The step opens on the catalog without a query.
    until(&mut dom, "the search box", |dom| control(dom, "Search ACP Registry").is_some()).await;
    until(&mut dom, "the server's answer", |dom| !rendered_text(dom).contains("Searching the registry...")).await;
    // The catalog could not reach its feed; the step says what the server said instead of an empty list.
    assert!(rendered_text(&dom).contains("error sending request for url (https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json)"), "{}", rendered_text(&dom));
    assert!(!rendered_text(&dom).contains("No compatible agents found"));
    assert!(control(&dom, "Add instance").is_none(), "an agent is added from its own button, not from the step");

    // "Enter manually" and "Local ACP command" go to the form of a command run on the environment.
    click_control(&mut dom, "Use a local ACP command");
    until(&mut dom, "the local command form", |dom| control(dom, "Add instance").is_some() && control(dom, "Search ACP Registry").is_none()).await;
    assert!(rendered_text(&dom).contains("Executable"), "{}", rendered_text(&dom));
    assert_eq!(crate::runtime::transport_tests::control(&dom, "Provider driver").and_then(|(_, value)| value).as_deref(), Some("acpRegistry"));
    fixture.finish(dom).await;
}
