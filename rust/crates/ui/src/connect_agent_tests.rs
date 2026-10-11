//! `/connect-agent` through the mounted page and the real native server's MCP OAuth routes: an agent's request is checked, and the
//! person approves with a pairing code or in one click, or denies; each ends in the address the browser is sent to, which is the
//! agent's own redirect with the code or the error. Nothing leaves loopback.
use super::diff_tests::{rendered_text, until, wait_for_rendered_text};
use super::*;
use crate::connect_agent::{Sink, Surface};
use crate::runtime::transport_tests::{api_fixture, click_control, click_when_enabled, control, control_attribute, input_control, start_fixture, until_enabled};
use std::{cell::RefCell, rc::Rc, sync::Arc};

const CHALLENGE: &str = "DeB41nTVkPwpbbYecrnqtVq7VXLezustdHAK4SWt13c";
const CALLBACK: &str = "http://localhost:51234/callback";

struct World {
    address: String,
    auth: t3_server::auth::AuthService,
    client_id: String,
    server: crate::runtime::fixture_server::Server,
    _directory: tempfile::TempDir,
}

async fn world() -> World {
    let directory = tempfile::Builder::new().prefix("t3code-rust-port-test-connect-agent-").tempdir().unwrap();
    let api = api_fixture(&directory, "agent-env");
    let (auth, store) = (api.auth.clone(), api.store.clone());
    let secrets = t3_server::server_secret_store::ServerSecretStore::open(directory.path().join("secrets")).unwrap();
    t3_server::mcp_oauth::install(&store, Arc::new(t3_server::mcp_oauth::McpOAuth::new(auth.clone(), &secrets).unwrap()));
    let (address, server) = start_fixture(api).await;
    let registered: Value = reqwest::Client::new()
        .post(format!("{address}/oauth/mcp/register"))
        .json(&json!({"client_name":"Claude Code","redirect_uris":[CALLBACK],"grant_types":["authorization_code","refresh_token"],"token_endpoint_auth_method":"none"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let client_id = registered["client_id"].as_str().unwrap().to_owned();
    World { address, auth, client_id, server, _directory: directory }
}

impl World {
    /// The query the server's authorize route hands the page.
    fn query(&self, client_id: &str) -> String {
        url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs([("response_type", "code"), ("client_id", client_id), ("redirect_uri", CALLBACK), ("code_challenge", CHALLENGE), ("code_challenge_method", "S256"), ("state", "state-1"), ("resource", &format!("{}/mcp", self.address))])
            .finish()
    }
    fn code(&self, scopes: &[AuthEnvironmentScope]) -> String {
        self.auth.create_pairing_credential(scopes, chrono::Utc::now(), chrono::Duration::minutes(5)).unwrap()
    }
    async fn cookie(&self, scopes: &[AuthEnvironmentScope]) -> String {
        let response = reqwest::Client::new().post(format!("{}/api/auth/browser-session", self.address)).json(&json!({"credential": self.code(scopes)})).send().await.unwrap();
        response.headers()["set-cookie"].to_str().unwrap().split(';').next().unwrap().to_owned()
    }
    fn page(&self, query: String, cookie: Option<String>) -> (VirtualDom, Rc<RefCell<Vec<String>>>) {
        #[derive(Clone)]
        struct Props(String, String, Option<String>);
        let sent = Rc::new(RefCell::new(Vec::new()));
        let mut dom = VirtualDom::new_with_props(|Props(origin, query, cookie): Props| rsx! { Surface { origin, query, cookie } }, Props(self.address.clone(), query, cookie));
        dom.provide_root_context(Sink(sent.clone()));
        dom.rebuild_in_place();
        (dom, sent)
    }
    async fn finish(self, dom: VirtualDom) {
        drop(dom);
        self.server.await;
    }
}

fn redirect(sent: &Rc<RefCell<Vec<String>>>) -> Option<url::Url> {
    sent.borrow().last().map(|address| url::Url::parse(address).unwrap())
}
fn param(url: &url::Url, name: &str) -> Option<String> {
    url.query_pairs().find(|(key, _)| key == name).map(|(_, value)| value.into_owned())
}
fn disabled(dom: &VirtualDom, label: &str) -> bool {
    control_attribute(dom, label, "disabled").unwrap_or_else(|| panic!("no control {label}: {}", rendered_text(dom))).1.as_deref() == Some("true")
}

const OPERATOR: &[AuthEnvironmentScope] = &[AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate];

#[tokio::test(flavor = "current_thread")]
async fn a_pairing_code_approves_the_agent_and_the_browser_returns_to_it_with_a_code() {
    let world = world().await;
    let (mut dom, sent) = world.page(world.query(&world.client_id), None);
    wait_for_rendered_text(&mut dom, "Connect Claude Code").await;
    assert!(rendered_text(&dom).contains(&format!("{}", world.address.trim_start_matches("http://"))), "the page names the environment");
    assert!(rendered_text(&dom).contains("Approval returns to localhost:51234 on the computer that opened this page."));
    // Nothing is approved without a code, and read only is the first choice.
    assert!(disabled(&dom, "Approve"));
    assert_eq!(control_attribute(&dom, "Read only", "checked").unwrap().1.as_deref(), Some("true"));
    click_control(&mut dom, "Auto");
    input_control(&mut dom, "Pairing code", "not-a-code");
    click_when_enabled(&mut dom, "Approve").await;
    wait_for_rendered_text(&mut dom, "unknown, expired, or already used").await;
    until_enabled(&mut dom, "Approve").await;
    assert!(sent.borrow().is_empty(), "a refused code goes nowhere");

    input_control(&mut dom, "Pairing code", &world.code(OPERATOR));
    click_when_enabled(&mut dom, "Approve").await;
    until(&mut dom, "the redirect", |_| !sent.borrow().is_empty()).await;
    let to = redirect(&sent).unwrap();
    assert_eq!((to.host_str(), to.path()), (Some("localhost"), "/callback"));
    assert!(param(&to, "code").is_some());
    assert_eq!(param(&to, "state").as_deref(), Some("state-1"));
    world.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn denying_sends_the_agent_access_denied_without_a_code() {
    let world = world().await;
    let (mut dom, sent) = world.page(world.query(&world.client_id), None);
    wait_for_rendered_text(&mut dom, "Connect Claude Code").await;
    click_when_enabled(&mut dom, "Deny").await;
    until(&mut dom, "the redirect", |_| !sent.borrow().is_empty()).await;
    let to = redirect(&sent).unwrap();
    assert_eq!((param(&to, "error").as_deref(), param(&to, "state").as_deref(), param(&to, "code")), (Some("access_denied"), Some("state-1"), None));
    world.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_request_from_a_client_the_server_did_not_register_cannot_continue() {
    let world = world().await;
    let (mut dom, sent) = world.page(world.query("forged-client"), None);
    wait_for_rendered_text(&mut dom, "This sign-in cannot continue").await;
    wait_for_rendered_text(&mut dom, "start the sign-in again from your agent").await;
    assert!(control(&dom, "Approve").is_none() && control(&dom, "Deny").is_none());
    assert!(sent.borrow().is_empty(), "a forged client is told nothing it could follow");
    world.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_request_with_a_protocol_error_goes_straight_back_to_the_agent_and_a_signed_in_administrator_approves_in_one_click() {
    let world = world().await;
    let broken = world.query(&world.client_id).replace("S256", "plain");
    let (mut dom, sent) = world.page(broken, None);
    until(&mut dom, "the redirect", |_| !sent.borrow().is_empty()).await;
    assert_eq!(param(&redirect(&sent).unwrap(), "error").as_deref(), Some("invalid_request"));
    drop(dom);

    let cookie = world.cookie(t3_contracts::AUTH_ADMINISTRATIVE_SCOPES).await;
    let (mut dom, sent) = world.page(world.query(&world.client_id), Some(cookie));
    wait_for_rendered_text(&mut dom, "Connect Claude Code").await;
    // The session holds every access, so no code is asked for, and Approve is ready at once.
    until(&mut dom, "no code asked for", |dom| control(dom, "Pairing code").is_none()).await;
    click_control(&mut dom, "Full access");
    click_when_enabled(&mut dom, "Approve").await;
    until(&mut dom, "the redirect", |_| !sent.borrow().is_empty()).await;
    assert!(param(&redirect(&sent).unwrap(), "code").is_some());
    world.finish(dom).await;
}
