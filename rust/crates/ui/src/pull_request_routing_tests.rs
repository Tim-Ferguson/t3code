//! Pull requests through another machine, against two real in-process servers and GitHub faked at its API (`fake_github`): the window is
//! on one server and a second is connected in the background, each with a checkout of the same repository. What the person has shared
//! on the Connections page decides whether a read or an act is asked of the second machine; it must be the same GitHub account, and an
//! act needs both machines on "Read and act". The fake host is one for the whole test binary, so these tests run one at a time.
use super::diff_tests::{Fixture, Harness, git, rendered_text, seed, until};
use super::environments_tests::Remote;
use super::fake_github::{Github, kind_of};
use super::source_control_tests::world;
use super::transport_tests::{change_control, control, drive_future};
use super::*;
use crate::{
    connection_store::{self, Backend, ReadFailure, Saved},
    diff_panel::Gate,
};
use std::cell::RefCell as Cell;
use t3_client::github_routing::Permission;

const WRITER: [AuthEnvironmentScope; 3] = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::SourceControlWrite];

/// Where the saved connections (and the shares among them) are kept in these tests: a string, as the browser keeps it.
#[derive(Default)]
struct Memory(Cell<Option<String>>);
impl Backend for Rc<Memory> {
    fn read(&self) -> Result<Option<String>, ReadFailure> {
        Ok(self.0.borrow().clone())
    }
    fn write(&self, document: &str) -> Result<(), String> {
        *self.0.borrow_mut() = Some(document.to_owned());
        Ok(())
    }
    fn set_aside(&self) {}
}

struct Pair {
    fixture: Fixture,
    other: Remote,
    dom: VirtualDom,
    props: Harness,
    state: Store<UiModel>,
    _github: Github,
    memory: Rc<Memory>,
    origin: EnvironmentId,
    _alone: tokio::sync::MutexGuard<'static, ()>,
}

/// The window on a server whose address is not this machine's own (so a read is not simply answered where it is asked), and a second
/// server connected in the background, both with a checkout of acme/web and the pull request at the fake host.
async fn pair() -> Pair {
    pair_with("https://github.com/acme/web.git").await
}

/// As `pair`, with the second server's checkout of the repository cloned from `remote`.
async fn pair_with(remote: &str) -> Pair {
    let world = world();
    let alone = world.exclusive().lock().await;
    world.reset_everything_signed_in();
    let github = Github::install(world, 7);
    let fixture = Fixture::start(true).await;
    git(&fixture.workspace, &["remote", "add", "origin", "https://github.com/acme/web.git"]);
    git(&fixture.workspace, &["checkout", "-q", "-b", "feature"]);
    // 0.0.0.0 reaches this machine's server but is not a loopback name, which is what makes a server "remote" to the router.
    let (mut dom, props) = fixture.client_at(Gate::default(), &WRITER, &fixture.address.replace("127.0.0.1", "0.0.0.0"));
    let mut state = seed(&mut dom, &props, &fixture.workspace).await;
    let memory = Rc::new(Memory::default());
    state.write().saved = Saved::in_backend(Rc::new(memory.clone()));
    connection_store::hydrate(state).await;
    let other = Remote::start_writing("env-b", "Beta host").await;
    let (transport, address, token) = (props.transport.clone(), other.address.clone(), other.token.clone());
    dom.in_scope(ScopeId::APP, || {
        spawn(async move { connect_in_background(transport, state, address, token).await });
    });
    let other_id = other.id.clone();
    until(&mut dom, "the second environment", |_| state.peek().parked.get(&other_id).is_some_and(|slice| slice.status == ConnectionStatus::Connected && slice.shell.synchronized)).await;
    let link = props.transport.link_of(&other.id).expect("a connected environment");
    let create = other.checkout_project("project-b", "Beta project", remote);
    dom.in_scope(ScopeId::APP, || request(&link, state, "projects.mutate", create, RequestKind::Unary)).expect("accepted");
    until(&mut dom, "the second's project", |_| state.peek().parked.get(&other_id).is_some_and(|slice| slice.shell.snapshot.as_ref().is_some_and(|snapshot| !snapshot.projects.is_empty()))).await;
    let origin = state.peek().destination.clone().expect("the window is on an environment");
    Pair { fixture, other, dom, props, state, _github: github, memory, origin, _alone: alone }
}

impl Pair {
    fn reference(&self) -> Value {
        json!({"projectId":"project","host":"github.com","repository":"acme/web","number":7})
    }
    /// Shares the machine (or not) the way the Connections page does.
    fn share(&self, environment: &EnvironmentId, permission: Permission) {
        connection_store::set_github_permission(self.state, environment, permission).expect("the share is kept");
    }
    /// A request about the pull request through the router, answered while the window runs.
    async fn ask(&mut self, method: &str, extra: Value) -> Result<Value, String> {
        let mut payload = self.reference();
        payload.as_object_mut().unwrap().extend(extra.as_object().cloned().unwrap_or_default());
        let (sender, receiver) = futures_channel::oneshot::channel();
        let (transport, state, method_name) = (self.props.transport.clone(), self.state, method.to_owned());
        self.dom.in_scope(ScopeId::APP, || {
            spawn(async move {
                let _ = sender.send(crate::pull_request_routing::request(transport, state, &method_name, payload).await);
            })
        });
        drive_future(&mut self.dom, method, async { receiver.await.unwrap() }).await
    }
    fn answered_by(&self) -> Vec<String> {
        crate::pull_request_routing::answered_by(self.origin.as_str(), &self.reference())
    }
    fn mutations(&self, kind: &str) -> usize {
        world().asked().iter().filter(|asked| kind_of(asked) == kind).count()
    }
    async fn finish(self) {
        let Self { fixture, other, dom, .. } = self;
        drop(dom);
        other.server.stop().await;
        fixture.finish_server().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_read_is_asked_of_the_other_machine_only_when_both_share_and_it_has_the_same_account() {
    let mut pair = pair().await;
    let (origin, other) = (pair.origin.clone(), pair.other.id.clone());
    // Nothing shared: the window's own server answers, and nothing is remembered.
    let own = pair.ask("pullRequests.summary", json!({})).await.unwrap();
    assert_eq!(own["number"], 7);
    assert!(pair.answered_by().is_empty());
    // Sharing is two-sided: a machine that is on while the other is off is not used.
    pair.share(&origin, Permission::Read);
    pair.ask("pullRequests.summary", json!({})).await.unwrap();
    assert!(pair.answered_by().is_empty());
    // Both on, the same account: the nearer machine (the second is on this machine; the window's is not) answers, with the window's
    // project id kept on the answer.
    pair.share(&other, Permission::Read);
    let routed = pair.ask("pullRequests.detail", json!({})).await.unwrap();
    assert_eq!((routed["number"].clone(), routed["projectId"].clone()), (json!(7), json!("project")));
    assert_eq!(pair.answered_by(), ["env-b"]);
    // The share is in the saved connections: a window started again over the same storage still has it.
    let mut state = pair.state;
    state.write().saved = Saved::in_backend(Rc::new(pair.memory.clone()));
    connection_store::hydrate(pair.state).await;
    assert_eq!(connection_store::github_permission(pair.state, &other), Permission::Read);
    pair.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_machine_that_cannot_vouch_for_the_host_is_passed_over_and_the_window_s_own_server_answers() {
    // The second machine's checkout is of a repository on another host: it has no account at github.com to prove.
    let mut pair = pair_with("https://gitlab.com/acme/web.git").await;
    let (origin, other) = (pair.origin.clone(), pair.other.id.clone());
    pair.share(&origin, Permission::Read);
    pair.share(&other, Permission::Read);
    let answer = pair.ask("pullRequests.summary", json!({})).await.unwrap();
    assert_eq!(answer["number"], 7);
    assert_eq!(pair.answered_by(), [pair.origin.to_string()], "the second machine could not vouch for the account and was passed over");
    pair.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn an_act_needs_both_machines_on_read_and_act_and_goes_through_the_other_one() {
    let mut pair = pair().await;
    let (origin, other) = (pair.origin.clone(), pair.other.id.clone());
    // Read-only sharing does not let an act through: it is done by the window's own server.
    pair.share(&origin, Permission::Read);
    pair.share(&other, Permission::Read);
    pair.ask("pullRequests.comment", json!({"body": "from here"})).await.unwrap();
    assert_eq!(pair.mutations("comment"), 1);
    assert!(pair.answered_by().is_empty());
    // One machine on "Read and act" is not enough either.
    pair.share(&origin, Permission::ReadWrite);
    pair.ask("pullRequests.comment", json!({"body": "still here"})).await.unwrap();
    assert!(pair.answered_by().is_empty());
    // Both: the second machine posts it (once), and the change reaches the caches of both.
    pair.share(&other, Permission::ReadWrite);
    pair.ask("pullRequests.comment", json!({"body": "from the other machine"})).await.unwrap();
    assert_eq!(pair.mutations("comment"), 3, "each act is done once");
    assert_eq!(pair.answered_by(), ["env-b"]);
    pair.ask("pullRequests.invalidate", json!({"reference": pair.reference()})).await.unwrap();
    pair.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_link_preview_resolves_through_the_router_like_any_other_read() {
    let mut pair = pair().await;
    let (origin, other) = (pair.origin.clone(), pair.other.id.clone());
    pair.share(&origin, Permission::Read);
    pair.share(&other, Permission::Read);
    let preview = pair.ask("pullRequests.preview", json!({})).await.unwrap();
    assert_eq!((preview["number"].clone(), preview["url"].clone()), (json!(7), json!("https://github.com/acme/web/pull/7")));
    assert_eq!(pair.answered_by(), ["env-b"]);
    pair.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn the_connections_page_shows_who_shares_and_a_changed_address_takes_the_share_back() {
    let mut pair = pair().await;
    let (origin, other) = (pair.origin.clone(), pair.other.id.clone());
    pair.state.view().set(View::Connections);
    until(&mut pair.dom, "the section", |dom| control(dom, "Beta host GitHub routing").is_some()).await;
    assert!(rendered_text(&pair.dom).contains("GitHub sharing") && rendered_text(&pair.dom).contains("Off"));
    let own = crate::fleet::label(pair.state, &origin);
    change_control(&mut pair.dom, "Beta host GitHub routing", "read-write");
    change_control(&mut pair.dom, &format!("{own} GitHub routing"), "read");
    until(&mut pair.dom, "the summary", |dom| rendered_text(dom).contains("Beta host read and act")).await;
    assert_eq!((connection_store::github_permission(pair.state, &other), connection_store::github_permission(pair.state, &origin)), (Permission::ReadWrite, Permission::Read));
    // What was kept is the grant with the key of the address it was given for, in the saved connections' document.
    let kept = pair.memory.0.borrow().clone().expect("the document was written");
    let kept: Value = serde_json::from_str(&kept).unwrap();
    let grants = kept["githubRoutingPermissions"].as_array().unwrap();
    assert_eq!(grants.len(), 2);
    assert!(grants.iter().any(|grant| grant["environmentId"] == "env-b" && grant["permission"] == "read-write" && grant["connectionKey"].as_str().is_some_and(|key| key.contains("PrimaryConnectionTarget"))));
    // An address saved for the machine is another endpoint: the trust given for the old one does not carry over.
    connection_store::remember(pair.state, &pair.other.address, &other, "Beta host", &pair.other.token);
    assert_eq!(connection_store::github_permission(pair.state, &other), Permission::Off, "trust was given to the address it was shown for");
    until(&mut pair.dom, "the summary without it", |dom| !rendered_text(dom).contains("Beta host read and act")).await;
    // A machine forgotten takes its grant with it.
    pair.share(&other, Permission::Read);
    connection_store::forget(pair.state, &other);
    assert!(!pair.memory.0.borrow().clone().unwrap().contains("env-b"));
    pair.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_pull_request_link_in_a_message_is_opened_in_the_app_after_the_router_resolved_it() {
    use super::diff_tests::open_thread;
    use super::transport_tests::click_control;
    let mut pair = pair().await;
    let (origin, other) = (pair.origin.clone(), pair.other.id.clone());
    pair.share(&origin, Permission::Read);
    pair.share(&other, Permission::Read);
    open_thread(&mut pair.dom, &pair.props, pair.state, "thread-a").await;
    let (state, transport) = (pair.state, pair.props.transport.clone());
    pair.dom.in_scope(ScopeId::APP, || {
        spawn(async move { crate::link_menus::external_link(state, Some(transport), Some("thread-a".to_owned()), "https://github.com/acme/web/pull/7".to_owned(), (3.0, 3.0)).await });
    });
    until(&mut pair.dom, "the link menu", |dom| control(dom, "Link actions").is_some() && control(dom, "Open pull request").is_some()).await;
    click_control(&mut pair.dom, "Open pull request");
    // The preview was asked of the machine that may answer, and the panel is then on the pull request.
    until(&mut pair.dom, "the panel", |dom| rendered_text(dom).contains("Fix it")).await;
    assert_eq!(pair.answered_by(), ["env-b"]);
    pair.finish().await;
}
