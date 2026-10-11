//! A pairing link (`/pair#token=…`, made by the Authorized clients list) opened on the server's own address, through the
//! mounted UI and the real native HTTP/WebSocket server: the credential it carries pairs this client and the app connects;
//! one the server does not know leaves the client blocked with the server's words. The address handling (taking the
//! credential out of the page's address) is covered by the pure tests of `t3_client::access`; here only the pairing is.
use super::*;
use crate::runtime::diff_tests::until;
use crate::runtime::transport_tests::{api_fixture, start_fixture};
use std::{cell::RefCell, rc::Rc};

#[derive(Clone)]
struct Harness {
    state: Rc<RefCell<Option<Store<UiModel>>>>,
    transport: TransportHandle,
    address: String,
    credential: String,
}
fn harness(props: Harness) -> Element {
    let state = use_store(UiModel::default);
    crate::themes::use_themes(state);
    *props.state.borrow_mut() = Some(state);
    let transport = props.transport.clone();
    use_future(move || {
        let transport = transport.clone();
        let address = props.address.clone();
        let credential = props.credential.clone();
        async move { pair_from_link(transport, state, address, credential).await }
    });
    rsx! {crate::Application {state,transport:props.transport}}
}

#[tokio::test(flavor = "current_thread")]
async fn a_pairing_link_pairs_this_client_and_a_credential_the_server_does_not_know_does_not() {
    let directory = tempfile::tempdir().unwrap();
    let api = api_fixture(&directory, "pair-link-ui-test");
    let credential = api
        .auth
        .create_pairing_credential(
            &[AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate],
            chrono::Utc::now(),
            chrono::Duration::minutes(1),
        )
        .unwrap();
    let (address, server) = start_fixture(api).await;

    let good = Harness { state: Rc::new(RefCell::new(None)), transport: TransportHandle::default(), address: address.clone(), credential };
    let mut dom = VirtualDom::new_with_props(harness, good.clone());
    dom.rebuild_in_place();
    let state = good.state.borrow().unwrap();
    until(&mut dom, "paired from the link", |_| state.peek().shell.synchronized).await;
    assert_eq!(state.peek().status, ConnectionStatus::Connected);
    assert!(state.peek().grants.authenticated);
    assert_eq!(state.peek().destination.as_ref().unwrap().as_str(), "pair-link-ui-test");
    drop(dom);

    let bad = Harness { state: Rc::new(RefCell::new(None)), transport: TransportHandle::default(), address, credential: "not-a-credential".into() };
    let mut dom = VirtualDom::new_with_props(harness, bad.clone());
    dom.rebuild_in_place();
    let state = bad.state.borrow().unwrap();
    until(&mut dom, "refused", |_| matches!(state.peek().status, ConnectionStatus::Blocked(_))).await;
    assert!(!state.peek().shell.synchronized);
    drop(dom);
    server.await;
}
