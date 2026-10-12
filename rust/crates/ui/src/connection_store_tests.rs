//! The saved connections through the mounted Connections page and real in-process servers: a paired server is saved with its token
//! in an encrypted file (nothing readable on disk, nothing on the page), comes back after a restart and connects without pairing
//! again, can be switched off and forgotten, and a catalog that cannot be read is set aside rather than overwritten. The key store
//! is a file in the test's own temporary directory: no test reaches the keychain.
use super::diff_tests::{until, wait_for_rendered_text};
use super::fixture_server::Server;
use super::transport_tests::{api_fixture, click_control, control, rendered_text, start_fixture};
use super::*;
use crate::connection_store::{Backend, ReadFailure, Saved};
use std::path::Path;
use t3_desktop::catalog_vault::{FileKey, Vault};

/// A backend over the encrypted file in `directory`, as the desktop app makes it, but with the key beside it.
fn encrypted(directory: &Path) -> Rc<dyn Backend> {
    Rc::new(crate::connection_store::Encrypted(Vault::new(directory.join("connection-catalog.json"), Arc::new(FileKey(directory.join("connection-catalog.key"))))))
}
use std::sync::Arc;

/// What the mounted app is asked to do once it has read the saved connections.
#[derive(Clone)]
enum Start {
    Nothing,
    /// Pair with a credential the person typed in, saving the connection.
    Pair { address: String, credential: String, remember: bool },
    /// What the app does at start: connect the saved environments in the background, then the window's own connection.
    Launch { address: String, credential: String },
}

#[derive(Clone)]
struct App {
    state: Rc<RefCell<Option<Store<UiModel>>>>,
    transport: TransportHandle,
    backend: Rc<dyn Backend>,
    start: Start,
}
fn app(props: App) -> Element {
    let state = use_store(|| UiModel { saved: Saved::in_backend(props.backend.clone()), view: View::Connections, ..UiModel::default() });
    crate::themes::use_themes(state);
    *props.state.borrow_mut() = Some(state);
    let transport = props.transport.clone();
    use_future(move || {
        let (transport, start) = (transport.clone(), props.start.clone());
        async move {
            crate::connection_store::hydrate(state).await;
            match start {
                Start::Nothing => {}
                Start::Pair { address, credential, remember: true } => pair_and_remember(transport, state, address, credential).await,
                Start::Pair { address, credential, remember: false } => pair_and_connect(transport, state, address, credential).await,
                Start::Launch { address, credential } => {
                    crate::connection_store::connect_saved(&transport, state);
                    pair_and_connect(transport, state, address, credential).await
                }
            }
        }
    });
    rsx! { crate::Application { state, transport: props.transport } }
}

fn mount(backend: Rc<dyn Backend>, start: Start) -> (VirtualDom, App, Store<UiModel>) {
    let props = App { state: Rc::new(RefCell::new(None)), transport: TransportHandle::default(), backend, start };
    let mut dom = VirtualDom::new_with_props(app, props.clone());
    dom.rebuild_in_place();
    let state = props.state.borrow().unwrap();
    (dom, props, state)
}

/// A server of its own, and a one-use credential for pairing with it.
struct Remote {
    address: String,
    credential: String,
    _directory: tempfile::TempDir,
    server: Server,
}
impl Remote {
    async fn start(environment: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let api = api_fixture(&directory, environment);
        let credential = api.auth.create_pairing_credential(&[AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate], chrono::Utc::now(), chrono::Duration::minutes(5)).unwrap();
        let (address, server) = start_fixture(api).await;
        Self { address, credential, _directory: directory, server }
    }
}

fn connected(dom: &VirtualDom, state: Store<UiModel>) -> bool {
    let _ = dom;
    state.peek().status == ConnectionStatus::Connected && state.peek().shell.synchronized
}

#[tokio::test(flavor = "current_thread")]
async fn a_paired_server_is_saved_encrypted_and_connects_after_a_restart_without_pairing_again() {
    let remote = Remote::start("saved-environment").await;
    let directory = tempfile::tempdir().unwrap();

    // The first run: the person pairs it from the Connections page's form.
    let (mut dom, props, state) = mount(encrypted(directory.path()), Start::Pair { address: remote.address.clone(), credential: remote.credential.clone(), remember: true });
    until(&mut dom, "the first connection", |dom| connected(dom, state)).await;
    let token = props.transport.borrow().bearer_token.clone();
    assert!(!token.is_empty());
    let on_disk = std::fs::read_to_string(directory.path().join("connection-catalog.json")).unwrap();
    for secret in [token.as_str(), remote.credential.as_str(), "saved-environment", "bearer"] {
        assert!(!on_disk.contains(secret), "the file is readable: {secret}");
    }
    let text = rendered_text(&dom);
    assert!(text.contains("Saved connections") && text.contains("Isolated integration"), "{text}");
    for secret in [token.as_str(), remote.credential.as_str()] {
        assert!(!text.contains(secret), "a secret is on the page");
    }
    // The catalog holds what the original's holds: one direct route with its profile and its credential.
    let decrypted = Vault::new(directory.path().join("connection-catalog.json"), Arc::new(FileKey(directory.path().join("connection-catalog.key")))).read().unwrap().unwrap();
    let document: Value = serde_json::from_str(&decrypted).unwrap();
    assert_eq!(document["targets"][0]["_tag"], "BearerConnectionTarget");
    assert_eq!(document["profiles"][0]["httpBaseUrl"], format!("{}/", remote.address));
    assert_eq!(document["credentials"][0]["credential"]["token"], token);
    drop(dom);

    // The next run: nothing is typed. The server is listed from the file, and Connect uses the saved token.
    let (mut dom, props, state) = mount(encrypted(directory.path()), Start::Nothing);
    until(&mut dom, "the saved connection", |dom| control(dom, "Connect Isolated integration").is_some()).await;
    assert_eq!(state.peek().status, ConnectionStatus::Disconnected);
    assert!(rendered_text(&dom).contains(&format!("{}/", remote.address)));
    assert!(!rendered_text(&dom).contains(&token));
    click_control(&mut dom, "Connect Isolated integration");
    until(&mut dom, "the connection from the saved token", |dom| connected(dom, state)).await;
    assert_eq!(state.peek().destination.as_ref().map(|id| id.as_str()), Some("saved-environment"));
    assert!(state.peek().grants.authenticated);
    assert_eq!(props.transport.borrow().bearer_token, token, "the same session, not a new pairing");

    // Switching it off keeps it and its token but takes away the way to connect it.
    click_control(&mut dom, "Switch off Isolated integration");
    wait_for_rendered_text(&mut dom, "Switched off.").await;
    assert_eq!(control_attribute_of(&dom, "Connect Isolated integration", "disabled").as_deref(), Some("true"));
    let document = read_catalog(directory.path());
    assert_eq!(document["disabledEnvironmentIds"][0], "saved-environment");
    assert_eq!(document["credentials"][0]["credential"]["token"], token);
    click_control(&mut dom, "Switch on Isolated integration");
    wait_for_rendered_text(&mut dom, "Switch off").await;
    assert_ne!(control_attribute_of(&dom, "Connect Isolated integration", "disabled").as_deref(), Some("true"));

    // Forgetting removes the routes, the profile and the token from the file, and the row from the page.
    click_control(&mut dom, "Forget Isolated integration");
    until(&mut dom, "the row to go", |dom| control(dom, "Connect Isolated integration").is_none()).await;
    let document = read_catalog(directory.path());
    assert_eq!((document["targets"].as_array().unwrap().len(), document["profiles"].as_array().unwrap().len(), document["credentials"].as_array().unwrap().len()), (0, 0, 0));
    assert!(!std::fs::read_to_string(directory.path().join("connection-catalog.json")).unwrap().contains(&token));
    drop(dom);
    remote.server.stop().await;
}

fn control_attribute_of(dom: &VirtualDom, label: &str, attribute: &str) -> Option<String> {
    super::transport_tests::control_attribute(dom, label, attribute).and_then(|(_, value)| value)
}
fn read_catalog(directory: &Path) -> Value {
    let vault = Vault::new(directory.join("connection-catalog.json"), Arc::new(FileKey(directory.join("connection-catalog.key"))));
    serde_json::from_str(&vault.read().unwrap().unwrap()).unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn a_connection_that_is_not_the_persons_to_keep_is_not_saved() {
    // The server this app starts for itself, and an SSH tunnel, are paired for one run: `pair_and_connect` saves nothing.
    let remote = Remote::start("embedded").await;
    let directory = tempfile::tempdir().unwrap();
    let (mut dom, _props, state) = mount(encrypted(directory.path()), Start::Pair { address: remote.address.clone(), credential: remote.credential.clone(), remember: false });
    until(&mut dom, "the connection", |dom| connected(dom, state)).await;
    assert!(!directory.path().join("connection-catalog.json").exists());
    assert!(!rendered_text(&dom).contains("Connect Isolated integration"));
    drop(dom);
    remote.server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_saved_address_stays_pinned_to_its_environment_across_a_restart() {
    // A catalog written by an earlier run says this address is `saved-environment`; the server there is another one now. The token
    // is never sent to it.
    let remote = Remote::start("replacement-environment").await;
    let directory = tempfile::tempdir().unwrap();
    let backend = encrypted(directory.path());
    let document = {
        let mut document = t3_client::catalog::Document::default();
        document.register(t3_client::catalog::bearer_registration(&EnvironmentId::new("saved-environment").unwrap(), "Saved", &format!("{}/", remote.address), "token-of-the-old-server"), None);
        document
    };
    backend.write(&document.encode()).unwrap();
    let (mut dom, props, state) = mount(backend, Start::Nothing);
    until(&mut dom, "the saved connection", |dom| control(dom, "Connect Saved").is_some()).await;
    click_control(&mut dom, "Connect Saved");
    until(&mut dom, "the refusal", |_| matches!(state.peek().status, ConnectionStatus::Blocked(_))).await;
    assert!(state.peek().error.as_ref().is_some_and(|message| message.contains("different environment")), "{:?}", state.peek().error);
    assert!(props.transport.borrow().sender.is_none() && props.transport.borrow().bearer_token.is_empty());
    drop(dom);
    remote.server.stop().await;
}

/// A store whose document is not readable, or which the platform will not open.
struct Unreadable(Result<(), &'static str>, std::cell::Cell<bool>, std::cell::Cell<bool>);
impl Backend for Unreadable {
    fn read(&self) -> Result<Option<String>, ReadFailure> {
        match self.0 {
            Ok(()) => Err(ReadFailure::Unreadable("The saved connections file is damaged.".into())),
            Err(cause) => Err(ReadFailure::Unavailable(cause.into())),
        }
    }
    fn write(&self, _: &str) -> Result<(), String> {
        self.1.set(true);
        Ok(())
    }
    fn set_aside(&self) {
        self.2.set(true);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_catalog_that_cannot_be_read_is_set_aside_and_one_the_platform_will_not_open_is_never_overwritten() {
    let remote = Remote::start("saved-environment").await;

    // Damaged: set aside (kept), said so, and the app starts empty and can save afresh.
    let backend = Rc::new(Unreadable(Ok(()), Default::default(), Default::default()));
    let (mut dom, _props, state) = mount(backend.clone(), Start::Nothing);
    until(&mut dom, "the notice", |dom| rendered_text(dom).contains("They were set aside, not deleted")).await;
    assert!(backend.2.get(), "set aside");
    assert!(state.peek().saved.entries().is_empty());
    drop(dom);

    // The platform would not give the key: nothing is set aside, and a pairing finished now is not written over what is there.
    let backend = Rc::new(Unreadable(Err("The keychain is locked."), Default::default(), Default::default()));
    let (mut dom, _props, state) = mount(backend.clone(), Start::Pair { address: remote.address.clone(), credential: remote.credential.clone(), remember: true });
    until(&mut dom, "the connection", |dom| connected(dom, state)).await;
    until(&mut dom, "the notice", |dom| rendered_text(dom).contains("The keychain is locked.") && rendered_text(dom).contains("This connection was not saved")).await;
    assert!(!backend.2.get(), "not set aside");
    assert!(!backend.1.get(), "nothing was written over the unread document");
    drop(dom);
    remote.server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn saved_environments_connect_in_the_background_at_start_while_the_window_opens_on_its_own() {
    let (saved, own) = (Remote::start("saved-environment").await, Remote::start("own-environment").await);
    let directory = tempfile::tempdir().unwrap();
    let (mut dom, _props, state) = mount(encrypted(directory.path()), Start::Pair { address: saved.address.clone(), credential: saved.credential.clone(), remember: true });
    until(&mut dom, "the first run", |dom| connected(dom, state)).await;
    drop(dom);

    // The next run opens the window on another server; nothing is typed for the saved one.
    let (mut dom, _props, state) = mount(encrypted(directory.path()), Start::Launch { address: own.address.clone(), credential: own.credential.clone() });
    let saved_id = EnvironmentId::new("saved-environment").unwrap();
    until(&mut dom, "both", |dom| connected(dom, state) && state.peek().parked.get(&saved_id).is_some_and(|slice| slice.status == ConnectionStatus::Connected && slice.shell.synchronized)).await;
    assert_eq!(state.peek().destination.as_ref().map(|id| id.as_str()), Some("own-environment"), "the window is on its own server");
    assert_eq!(state.peek().live.get(&saved_id), Some(&ConnectionStatus::Connected));
    // Only the saved server is in the file: the window's own connection is not the person's to keep.
    assert_eq!(read_catalog(directory.path())["targets"].as_array().unwrap().len(), 1);
    assert!(control(&dom, "Open Isolated integration").is_some());
    drop(dom);
    saved.server.stop().await;
    own.server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_saved_server_that_cannot_be_reached_is_listed_as_lost_retried_on_request_and_ended_by_switching_it_off() {
    let own = Remote::start("own-environment").await;
    let directory = tempfile::tempdir().unwrap();
    let backend = encrypted(directory.path());
    let away = EnvironmentId::new("away-environment").unwrap();
    let dead = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}/", listener.local_addr().unwrap())
    };
    let mut document = t3_client::catalog::Document::default();
    document.register(t3_client::catalog::bearer_registration(&away, "Away", &dead, "token-of-away"), None);
    backend.write(&document.encode()).unwrap();

    let (mut dom, props, state) = mount(backend, Start::Launch { address: own.address.clone(), credential: own.credential.clone() });
    until(&mut dom, "the window's own server", |dom| connected(dom, state)).await;
    until(&mut dom, "the lost server in the list", |dom| control(dom, "Retry Away").is_some()).await;
    let text = rendered_text(&dom);
    assert!(text.contains("Disconnected. Reconnecting"), "{text}");
    assert!(matches!(state.peek().live.get(&away), Some(ConnectionStatus::Interrupted(_))));
    assert!(!text.contains("token-of-away"));
    assert!(props.transport.link_of(&away).is_some(), "its connection is kept and tried again");

    // Switching it off ends the attempts and keeps it saved.
    click_control(&mut dom, "Switch off Away");
    assert!(props.transport.link_of(&away).is_none() && !state.peek().live.contains_key(&away));
    until(&mut dom, "the saved server switched off", |dom| rendered_text(dom).contains("Switched off.")).await;
    assert_eq!(read_catalog(directory.path())["disabledEnvironmentIds"][0], "away-environment");
    drop(dom);
    own.server.stop().await;
}
