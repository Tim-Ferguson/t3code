//! "Authorized clients" through the mounted UI and the real native server: a pairing link made here, copied, used by
//! another device (which then appears as a client), and revoked, as a link and as a client; revoking every other
//! client; and what a connection without the access permissions sees.
use super::diff_tests::{Fixture, rendered_text, seed, until, wait_for_rendered_text};
use super::*;
use crate::access_page::AdvertisedEndpoints;
use crate::diff_panel::Gate;
use crate::runtime::transport_tests::{click_control, control, control_attribute, input_control};

const ADMIN: &[AuthEnvironmentScope] = &[
    AuthEnvironmentScope::OrchestrationRead,
    AuthEnvironmentScope::OrchestrationOperate,
    AuthEnvironmentScope::AccessRead,
    AuthEnvironmentScope::AccessWrite,
    AuthEnvironmentScope::FilesystemRead,
    AuthEnvironmentScope::DiagnosticsRead,
    AuthEnvironmentScope::TerminalRead,
];

/// What the other device does with the credential from the link.
async fn pair(address: &str, credential: &str) {
    let body = format!(
        "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Atoken-exchange&subject_token={credential}&subject_token_type=urn%3At3%3Aparams%3Aoauth%3Atoken-type%3Aenvironment-bootstrap&requested_token_type=urn%3Aietf%3Aparams%3Aoauth%3Atoken-type%3Aaccess_token&client_device_type=mobile&client_label=Second%20phone"
    );
    let response = reqwest::Client::new().post(format!("{address}/oauth/token")).header("content-type", "application/x-www-form-urlencoded").body(body).send().await.unwrap();
    assert!(response.status().is_success(), "{}", response.text().await.unwrap_or_default());
}

#[tokio::test(flavor = "current_thread")]
async fn a_pairing_link_is_created_used_by_another_device_and_revoked_and_so_is_the_client() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.client(Gate::default(), ADMIN);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    state.view().set(View::Connections);
    wait_for_rendered_text(&mut dom, "1 client").await;
    assert_eq!(control_attribute(&dom, "Revoke others", "disabled").unwrap().1.as_deref(), Some("true"), "nobody else is signed in");

    // Create a read-only link with a label.
    click_control(&mut dom, "Create link");
    until(&mut dom, "the dialog", |dom| control(dom, "Client label").is_some()).await;
    input_control(&mut dom, "Client label", "Kitchen iPad");
    click_control(&mut dom, "Read only");
    assert_eq!(control_attribute(&dom, "View environment", "checked").unwrap().1.as_deref(), Some("true"));
    assert_eq!(control_attribute(&dom, "Operate tasks", "checked").unwrap().1.as_deref(), Some("false"));
    click_control(&mut dom, "Create");
    wait_for_rendered_text(&mut dom, "1 client \u{b7} 1 pairing link").await;
    let link = control_attribute(&dom, "Pairing link for Kitchen iPad", "value").expect("the link can be copied from the window that made it").1.unwrap();
    assert!(link.starts_with(&format!("{}/pair#token=", fixture.address)), "{link}");
    assert!(control(&dom, "Copy link for Kitchen iPad").is_some());
    assert!(rendered_text(&dom).contains("Expires in 4m") || rendered_text(&dom).contains("Expires in 5m"));

    // The other device uses it: the link is spent and the device is a client under the link's label.
    pair(&fixture.address, link.split("token=").nth(1).unwrap()).await;
    // Nothing was done in this window: the server's own word that someone paired shows the new client under the link's
    // label, and the spent link is gone.
    wait_for_rendered_text(&mut dom, "2 clients").await;
    assert!(!rendered_text(&dom).contains("pairing link") && control(&dom, "Revoke link Kitchen iPad").is_none(), "{}", rendered_text(&dom));
    assert!(rendered_text(&dom).contains("Mobile"), "its device type is shown");
    crate::runtime::transport_tests::click_when_enabled(&mut dom, "Revoke client Kitchen iPad").await;
    until(&mut dom, "the client gone", |dom| control(dom, "Revoke client Kitchen iPad").is_none()).await;
    wait_for_rendered_text(&mut dom, "1 client").await;
    // Another device pairs through a second link; "Revoke others" removes it.
    click_control(&mut dom, "Create link");
    until(&mut dom, "the dialog", |dom| control(dom, "Create").is_some()).await;
    input_control(&mut dom, "Client label", "Guest laptop");
    click_control(&mut dom, "Create");
    until(&mut dom, "the second link", |dom| control(dom, "Pairing link for Guest laptop").is_some()).await;
    let second = control_attribute(&dom, "Pairing link for Guest laptop", "value").unwrap().1.unwrap();
    pair(&fixture.address, second.split("token=").nth(1).unwrap()).await;
    wait_for_rendered_text(&mut dom, "2 clients").await;
    crate::runtime::transport_tests::click_when_enabled(&mut dom, "Revoke others").await;
    wait_for_rendered_text(&mut dom, "1 client").await;
    assert!(!rendered_text(&dom).contains("Guest laptop"));
    assert_eq!(error_of(state), None);
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_link_that_nobody_used_is_revoked_from_the_list_and_the_client_row_marks_this_device() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.client(Gate::default(), ADMIN);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    state.view().set(View::Connections);
    wait_for_rendered_text(&mut dom, "This device").await;
    assert!(control(&dom, "Revoke client Browser fixture").is_none(), "this device cannot be revoked from itself");
    click_control(&mut dom, "Create link");
    until(&mut dom, "the dialog", |dom| control(dom, "Create").is_some()).await;
    click_control(&mut dom, "Create");
    until(&mut dom, "the link", |dom| control(dom, "Revoke link Pairing link").is_some()).await;
    click_control(&mut dom, "Revoke link Pairing link");
    until(&mut dom, "the link gone", |dom| control(dom, "Revoke link Pairing link").is_none()).await;
    assert!(rendered_text(&dom).contains("1 client") && !rendered_text(&dom).contains("pairing link"));
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_connection_that_cannot_read_access_sees_no_section_and_one_that_cannot_write_cannot_change_it() {
    let fixture = Fixture::start(false).await;
    let none = [AuthEnvironmentScope::OrchestrationRead];
    let (mut dom, _props) = fixture.client(Gate::default(), &none);
    let state = _props.state.borrow().unwrap();
    until(&mut dom, "the shell", |_| state.peek().shell.synchronized).await;
    state.view().set(View::Connections);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    assert!(!rendered_text(&dom).contains("Authorized clients"));
    fixture.finish(dom).await;

    let fixture = Fixture::start(false).await;
    let read = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::AccessRead];
    let (mut dom, props) = fixture.client(Gate::default(), &read);
    let state = props.state.borrow().unwrap();
    until(&mut dom, "the shell", |_| state.peek().shell.synchronized).await;
    state.view().set(View::Connections);
    wait_for_rendered_text(&mut dom, "1 client").await;
    for label in ["Create link", "Revoke others"] {
        assert_eq!(control_attribute(&dom, label, "disabled").unwrap().1.as_deref(), Some("true"), "{label}");
    }
    fixture.finish(dom).await;
}

fn endpoint(id: &str, label: &str, url: &str, reachability: &str, hosted: &str) -> t3_client::access::Endpoint {
    t3_client::access::Endpoint::parse(&json!({"id":id,"label":label,"provider":{"id":"desktop-core"},"httpBaseUrl":url,"reachability":reachability,"compatibility":{"hostedHttpsApp":hosted},"status":"available"})).unwrap()
}

/// Makes a link in the open window and returns its label's control names.
async fn link_in(dom: &mut VirtualDom, label: &str) {
    click_control(dom, "Create link");
    until(dom, "the dialog", |dom| control(dom, "Client label").is_some()).await;
    input_control(dom, "Client label", label);
    click_control(dom, "Create");
    until(dom, "the link", |dom| control(dom, &format!("Pairing link for {label}")).is_some()).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_link_is_shared_through_an_endpoint_with_a_qr_code_unless_the_endpoint_is_loopback() {
    let fixture = Fixture::start(false).await;
    let advertised = AdvertisedEndpoints {
        endpoints: vec![
            endpoint("desktop-loopback:3774", "This machine", "http://127.0.0.1:3774", "loopback", "mixed-content-blocked"),
            endpoint("desktop-lan:http://192.168.1.20:3774", "Local network", "http://192.168.1.20:3774", "lan", "mixed-content-blocked"),
            endpoint("tailscale-magicdns:box.ts.net", "Tailscale HTTPS", "https://box.ts.net/", "private-network", "compatible"),
        ],
        default_key: None,
    };
    let (mut dom, props) = fixture.client_with(Gate::default(), ADMIN, |dom| dom.provide_root_context(advertised.clone()));
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    state.view().set(View::Connections);
    wait_for_rendered_text(&mut dom, "1 client").await;
    // A link nobody holds the credential of cannot be shared.
    link_in(&mut dom, "Kitchen iPad").await;
    assert!(control(&dom, "Share pairing link Kitchen iPad").is_some());
    assert!(control(&dom, "Reach via Local network").is_none(), "closed until asked for");
    click_control(&mut dom, "Share pairing link Kitchen iPad");
    until(&mut dom, "the panel", |dom| control(dom, "Reach via Local network").is_some()).await;
    // It opens on the endpoint a code can be made for, not on the loopback one that comes first.
    assert_eq!(control_attribute(&dom, "Reach via Local network", "aria-checked").unwrap().1.as_deref(), Some("true"));
    assert_eq!(control_attribute(&dom, "Reach via This machine", "aria-checked").unwrap().1.as_deref(), Some("false"));
    let text = rendered_text(&dom);
    assert!(text.contains("Reach this machine via") && text.contains("Devices on the same network") && text.contains("Opens the hosted app, no install needed"), "{text}");
    let code = "Pairing link \u{2014} scan to open on another device";
    assert!(rendered_text(&dom).contains("http://192.168.1.20:3774/pair#token="), "{text}");
    let credential = control_attribute(&dom, "Pairing link for Kitchen iPad", "value").unwrap().1.unwrap().split("token=").nth(1).unwrap().to_owned();
    // The code drawn is the original's drawing of that very link: the same size, and the paths are proved against its encoder in `t3_client::qr`.
    let view_of = |link: &str| {
        let drawing = t3_client::qr::draw(link, t3_client::qr::Level::Medium, 1).unwrap();
        format!("0 0 {0} {0}", drawing.view)
    };
    let lan = format!("http://192.168.1.20:3774/pair#token={credential}");
    assert_eq!(control_attribute(&dom, code, "viewBox").unwrap().1, Some(view_of(&lan)));

    // The hosted app's link for the https endpoint, longer, so a bigger code.
    click_control(&mut dom, "Reach via Tailscale HTTPS");
    let hosted = format!("https://app.t3.codes/pair?host=https%3A%2F%2Fbox.ts.net%2F#token={credential}");
    until(&mut dom, "the hosted link", |dom| rendered_text(dom).contains(&hosted)).await;
    assert_eq!(control_attribute(&dom, code, "viewBox").unwrap().1, Some(view_of(&hosted)));
    assert_ne!(view_of(&hosted), view_of(&lan));
    // The loopback endpoint stays copyable and gets no code.
    click_control(&mut dom, "Reach via This machine");
    until(&mut dom, "no code", |dom| rendered_text(dom).contains("No QR for this endpoint.")).await;
    assert!(control(&dom, code).is_none());
    assert!(rendered_text(&dom).contains(&format!("http://127.0.0.1:3774/pair#token={credential}")));
    // Closing hides the panel.
    click_control(&mut dom, "Share pairing link Kitchen iPad");
    until(&mut dom, "closed", |dom| control(dom, "Reach via This machine").is_none()).await;
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_server_reached_on_loopback_with_no_other_endpoint_offers_no_share() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.client(Gate::default(), ADMIN);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    state.view().set(View::Connections);
    wait_for_rendered_text(&mut dom, "1 client").await;
    link_in(&mut dom, "Laptop").await;
    assert!(control(&dom, "Share pairing link Laptop").is_none(), "a loopback address cannot be dialed from another device");
    assert!(control(&dom, "Copy link for Laptop").is_some(), "the link itself is still copyable on this machine");
    fixture.finish(dom).await;
}

fn error_of(state: Store<UiModel>) -> Option<String> {
    state.peek().error.clone()
}
