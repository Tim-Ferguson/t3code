//! "Open in <editor>" for an environment of another machine through the mounted UI and a real in-process server: which editors are
//! offered (this computer's, not the server's), what the button says when no name reaches the machine, which name the link carries (an
//! SSH alias before what the server advertises), and that the system is handed the link and the server is asked nothing.
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::diff_tests::{Fixture, open_thread, seed, until};
use crate::runtime::transport_tests::{click_when_enabled, control, control_attribute, rendered_text};
use std::cell::RefCell;
use t3_contracts::{AuthEnvironmentScope, ForwardCompatibleArray, RemoteOpenTarget, RemoteOpenTargetKind, TrimmedNonEmptyString};

const SCOPES: [AuthEnvironmentScope; 2] = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate];

/// What the system was handed, and how many times this computer was asked which editors it has.
#[derive(Default)]
struct System {
    links: RefCell<Vec<String>>,
    refuse: std::cell::Cell<bool>,
    probes: std::cell::Cell<u32>,
}

fn install_for(own_address: &str, system: &Rc<System>, installed: &'static [&'static str]) {
    let (address, handed, asked) = (own_address.to_owned(), system.clone(), system.clone());
    install(Bridge {
        server_address: Rc::new(move || Some(address.clone())),
        open_link: Rc::new(move |url| {
            handed.links.borrow_mut().push(url.to_owned());
            !handed.refuse.get()
        }),
        probe: Rc::new(move || {
            asked.probes.set(asked.probes.get() + 1);
            Box::pin(std::future::ready(installed.iter().map(|id| (*id).to_owned()).collect()))
        }),
    });
}

async fn open_window(fixture: &Fixture) -> (VirtualDom, Store<UiModel>) {
    let (mut dom, props) = fixture.client(Gate::default(), &SCOPES);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "config", |_| state.peek().typed_config.is_some()).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    (dom, state)
}

fn advertise(state: Store<UiModel>, kind: RemoteOpenTargetKind, host: &str) {
    let mut config = state.typed_config();
    config.write().as_mut().unwrap().remote_open_targets = Some(ForwardCompatibleArray(vec![RemoteOpenTarget { kind, host: TrimmedNonEmptyString::new(host).unwrap() }]));
}

fn disabled(dom: &VirtualDom, label: &str) -> bool {
    control_attribute(dom, label, "disabled").is_some_and(|(_, value)| value.as_deref() == Some("true"))
}

#[tokio::test(flavor = "current_thread")]
async fn an_environment_of_another_machine_is_opened_by_a_link_for_the_editor_here_and_the_server_is_asked_nothing() {
    let fixture = Fixture::start(false).await;
    let system = Rc::new(System::default());
    // The app's own server is somewhere else: this window is on a remote machine.
    install_for("http://127.0.0.1:9/", &system, &["cursor", "vscode", "idea"]);
    let (mut dom, state) = open_window(&fixture).await;

    // No name reaches the machine: the button is there and says so.
    until(&mut dom, "the note", |dom| control(dom, "Open in editor").is_some()).await;
    assert!(disabled(&dom, "Open in editor"));
    assert_eq!(control_attribute(&dom, "Open in editor", "title").unwrap().1.as_deref(), Some("No SSH route to Isolated integration"));
    assert!(control(&dom, "Choose editor").is_none());

    // The server advertises a name: the editors of this computer are offered (not IntelliJ, which takes no link), in the table's order.
    advertise(state, RemoteOpenTargetKind::Tailscale, "box.tail1234.ts.net");
    until(&mut dom, "the editors", |dom| control(dom, "Open in Cursor").is_some()).await;
    assert_eq!(system.probes.get(), 1, "this computer is asked once");
    click_when_enabled(&mut dom, "Choose editor").await;
    until(&mut dom, "the menu", |dom| control(dom, "VS Code").is_some()).await;
    assert!(control(&dom, "Cursor").is_some() && control(&dom, "IntelliJ IDEA").is_none());
    assert!(rendered_text(&dom).contains("Opens over SSH. Needs your key on Isolated integration"));

    // Choosing one hands the system the link, with the advertised name and the thread's folder.
    click_when_enabled(&mut dom, "VS Code").await;
    let workspace = fixture.workspace.to_string_lossy().into_owned();
    let expected = t3_desktop::remote_open::build_remote_open_url("vscode", "box.tail1234.ts.net", &workspace).unwrap();
    assert_eq!(*system.links.borrow(), [expected.clone()]);
    assert!(expected.starts_with("vscode://vscode-remote/ssh-remote+box.tail1234.ts.net/"));
    until(&mut dom, "the choice remembered", |dom| control(dom, "Open in VS Code").is_some()).await;
    // The hint was for the first open only.
    click_when_enabled(&mut dom, "Choose editor").await;
    until(&mut dom, "the menu again", |dom| control(dom, "Cursor").is_some()).await;
    assert!(!rendered_text(&dom).contains("Opens over SSH"));

    // An SSH alias is what the person's own ssh reaches the machine by, and it beats the advertised name.
    let destination = state.peek().destination.clone().unwrap();
    state.ssh().write().active = Some(crate::ssh_environments::Active { target: t3_client::ssh::Target { alias: "devbox".into(), hostname: "devbox.example".into(), username: None, port: None }, environment: destination });
    click_when_enabled(&mut dom, "Open in VS Code").await;
    assert_eq!(system.links.borrow().last().unwrap(), &t3_desktop::remote_open::build_remote_open_url("vscode", "devbox", &workspace).unwrap());

    // A link the system refuses is not remembered as the editor opened.
    system.refuse.set(true);
    click_when_enabled(&mut dom, "Open in VS Code").await;
    assert_eq!(system.links.borrow().len(), 3);
    assert!(state.shell().read().snapshot.is_some(), "and the window is as it was");
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn the_apps_own_server_starts_the_editor_as_it_always_did() {
    let fixture = Fixture::start(false).await;
    let system = Rc::new(System::default());
    // The window is on the app's own server: the server's editors stand, and no link is handed to the system.
    install_for(&fixture.address, &system, &["cursor", "vscode"]);
    let (mut dom, state) = open_window(&fixture).await;
    assert_eq!(mode(state), Some(State::LocalExec));
    assert_eq!(editors(state), None);
    assert!(!links(state) && hint(state).is_none() && no_route(state).is_none());
    // Even with a name advertised: the server is here.
    advertise(state, RemoteOpenTargetKind::Mdns, "box.local");
    until(&mut dom, "still local", |_| mode(state) == Some(State::LocalExec)).await;
    assert_eq!(open(state, "/work/project", "vscode"), None, "the server is asked, as it always was");
    assert!(system.links.borrow().is_empty());
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn this_computer_having_none_of_the_editors_offers_vs_code_and_a_browser_offers_nothing_new() {
    let fixture = Fixture::start(false).await;
    let system = Rc::new(System::default());
    install_for("http://127.0.0.1:9/", &system, &["idea", "trae"]);
    let (mut dom, state) = open_window(&fixture).await;
    advertise(state, RemoteOpenTargetKind::Mdns, "box.local");
    until(&mut dom, "the fallback", |dom| control(dom, "Open in VS Code").is_some()).await;
    assert_eq!(dom.in_scope(ScopeId::APP, || editors(state)), Some(vec!["vscode".to_owned()]));
    fixture.finish(dom).await;

    // No bridge, no remote mode: the browser's picker is the server's.
    BRIDGE.with(|slot| *slot.borrow_mut() = None);
    let fixture = Fixture::start(false).await;
    let (dom, state) = open_window(&fixture).await;
    assert_eq!(mode(state), None);
    assert_eq!(editors(state), None);
    assert!(hint(state).is_none() && no_route(state).is_none() && !links(state));
    fixture.finish(dom).await;
}
