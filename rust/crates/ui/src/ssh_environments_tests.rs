//! SSH environments through the mounted UI and two real native servers: the one the window is paired with, and the one that
//! "runs on the remote host", which the tunnel (`t3-remote`'s manager, run by the desktop shell's service) reaches through a
//! fake `ssh` that answers as the remote scripts do. The real `ssh` and `~/.ssh` are never touched: hosts are discovered in an
//! isolated home directory, and no process is started. Waits are on text, controls and the password queue that only the awaited
//! step produces.
use super::diff_tests::{Fixture, rendered_text, until, wait_for_rendered_text};
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::transport_tests::{click_control, click_when_enabled, control, control_attribute, input_control, until_enabled};
use crate::ssh_environments::{GatewayHandle, Primary};
use crate::ssh_gateway::DesktopGateway;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use t3_desktop::ssh::{Config, Ssh};
use t3_remote::{
    ssh_auth::SshAuthHost,
    ssh_scripts::RemoteT3RunnerOptions,
    ssh_tunnel::{SshProcess, SshRuntime},
    test_support::ssh_fakes::{FakeProbe, FakeProcess, FakeSpawner, FixedPorts, RecordedCall},
};

const SCOPES: &[AuthEnvironmentScope] = &[AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate];

/// What the fake remote host does, and what it was asked.
struct Remote {
    /// The server on the host, and how it signs clients in.
    address: String,
    spawner: Arc<FakeSpawner>,
    stops: Arc<AtomicUsize>,
    host: Fixture,
    ssh: Ssh,
    home: tempfile::TempDir,
    saved: std::path::PathBuf,
}

/// `password` is what the host accepts (any login when none), after the first try, which has no password, is refused.
async fn remote(password: Option<&'static str>, config_hosts: &str) -> Remote {
    let host = Fixture::start_named("remote-ssh").await;
    let auth = host.auth();
    let address = host.address.clone();
    let port: u16 = address.rsplit(':').next().unwrap().parse().unwrap();
    let stops = Arc::new(AtomicUsize::new(0));
    let spawner = FakeSpawner::new({
        let stops = stops.clone();
        move |call: &RecordedCall| -> Arc<dyn SshProcess> {
            if call.has("-G") {
                return FakeProcess::finished("hostname devbox.example.com\nuser julius\nport 2222\n", "", 0);
            }
            if call.has("-N") {
                return FakeProcess::running();
            }
            let accepted = match password {
                None => true,
                Some(good) => call.env.get("T3_SSH_AUTH_SECRET").map(String::as_str) == Some(good),
            };
            if !accepted {
                return FakeProcess::finished("", "julius@devbox: Permission denied (publickey,password).\n", 255);
            }
            if call.has("--") {
                FakeProcess::finished("{\"remotePort\":3773,\"serverKind\":\"managed\"}\n", "", 0)
            } else if call.stdin.as_deref().is_some_and(|script| script.contains("auth pairing create")) {
                let credential = auth.create_pairing_credential(SCOPES, chrono::Utc::now(), chrono::Duration::minutes(1)).unwrap();
                FakeProcess::finished(&format!("{{\"credential\":\"{credential}\"}}\n"), "", 0)
            } else {
                stops.fetch_add(1, Ordering::SeqCst);
                FakeProcess::finished("{\"stopped\":true}\n", "", 0)
            }
        }
    });
    // The home directory whose `.ssh` host discovery reads, apart from the person's own.
    let home = tempfile::Builder::new().prefix("t3code-rust-port-test-ssh-home-").tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".ssh")).unwrap();
    std::fs::write(home.path().join(".ssh/config"), config_hosts).unwrap();
    let config = Config {
        ssh_executable: "ssh".into(),
        home_dir: Some(home.path().display().to_string()),
        prompt_timeout: std::time::Duration::from_secs(60),
        runner: RemoteT3RunnerOptions { archive_version: Some("1.2.3".into()), ..Default::default() },
    };
    let askpass = home.path().join("askpass");
    let ssh = Ssh::with_runtime(config, {
        let spawner = spawner.clone();
        move |prompt| SshRuntime {
            spawner,
            prompt,
            ports: Arc::new(FixedPorts(port)),
            probe: FakeProbe::ok(),
            auth_host: SshAuthHost { windows: false, process_display: None, temp_root: askpass },
            ssh_command: "ssh".into(),
        }
    })
    .unwrap();
    let saved = home.path().join("ssh-environments.json");
    Remote { address, spawner, stops, host, ssh, home, saved }
}

/// The window, paired with `fixture`'s server, on the Connections page, with `remote` behind the desktop shell's gateway.
async fn page(fixture: &Fixture, remote: &Remote) -> (VirtualDom, Store<UiModel>) {
    let primary = fixture.auth();
    let address = fixture.address.clone();
    let gateway = DesktopGateway {
        ssh: remote.ssh.clone(),
        saved: remote.saved.clone(),
        primary: Box::new(move || {
            let credential = primary.create_pairing_credential(SCOPES, chrono::Utc::now(), chrono::Duration::minutes(1)).unwrap();
            Some(Primary::Pair { address: address.clone(), credential })
        }),
    };
    let handle = GatewayHandle(std::rc::Rc::new(gateway));
    let (mut dom, props) = fixture.client_with(Gate::default(), SCOPES, move |dom| dom.provide_root_context(handle));
    let mut state = props.state.borrow().unwrap();
    until(&mut dom, "the shell", |_| state.peek().shell.synchronized).await;
    state.view().set(View::Connections);
    (dom, state)
}

fn destination(state: Store<UiModel>) -> Option<String> {
    state.peek().destination.as_ref().map(ToString::to_string)
}

fn launches(remote: &Remote) -> usize {
    remote.spawner.count(|call| call.has("--"))
}

async fn finish(fixture: Fixture, remote: Remote, dom: VirtualDom) {
    fixture.finish(dom).await;
    remote.host.finish_server().await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_discovered_host_connects_after_a_password_retry_the_window_switches_to_it_and_back_and_it_disconnects() {
    let fixture = Fixture::start(false).await;
    let remote = remote(Some("hunter2"), "Host devbox\n  HostName devbox.example.com\nHost staging\n").await;
    let (mut dom, state) = page(&fixture, &remote).await;
    assert_eq!(destination(state).as_deref(), Some("diff-ui"));

    // The hosts of the isolated home's ssh configuration are offered.
    until_enabled(&mut dom, "Use host staging").await;
    click_when_enabled(&mut dom, "Use host devbox").await;

    // The host refuses the first login, so ssh asks for a password; the first one is wrong.
    let queue = remote.ssh.prompts().watch();
    until(&mut dom, "the first question", |_| queue.borrow().len() == 1).await;
    let first = queue.borrow()[0].clone();
    assert_eq!((first.destination.as_str(), first.username.as_deref()), ("devbox", Some("julius")));
    until(&mut dom, "the first prompt", |dom| control_attribute(dom, "SSH password", "data-request").and_then(|(_, id)| id).as_deref() == Some(first.request_id.as_str())).await;
    wait_for_rendered_text(&mut dom, "SSH Password Required").await;
    assert!(rendered_text(&dom).contains("julius@devbox"), "the dialog names who asks");
    input_control(&mut dom, "SSH password", "wrong");
    click_control(&mut dom, "Continue");

    // It asks again for the next; the dialog shows the new question, and this time the password is right.
    until(&mut dom, "the second question", |_| queue.borrow().len() == 1 && queue.borrow()[0].request_id != first.request_id).await;
    let second = queue.borrow()[0].clone();
    until(&mut dom, "the second prompt", |dom| control_attribute(dom, "SSH password", "data-request").and_then(|(_, id)| id).as_deref() == Some(second.request_id.as_str())).await;
    input_control(&mut dom, "SSH password", "hunter2");
    click_control(&mut dom, "Continue");

    wait_for_rendered_text(&mut dom, "is ready over an SSH-managed tunnel.").await;
    until(&mut dom, "the window on the remote environment", |_| destination(state).as_deref() == Some("remote-ssh") && state.peek().status == ConnectionStatus::Connected).await;
    {
        let model = state.peek();
        let active = model.ssh.active.as_ref().expect("the switch is recorded");
        assert_eq!((active.target.alias.as_str(), active.target.hostname.as_str(), active.target.port), ("devbox", "devbox.example.com", Some(2222)));
        assert_eq!(model.ssh.saved.len(), 1);
    }
    // The host was asked as the original asks it: launch, tunnel, pairing credential, in that order, over the one ssh.
    let calls = remote.spawner.recorded();
    let kind = |call: &RecordedCall| if call.has("-G") { "resolve" } else if call.has("-N") { "tunnel" } else if call.has("--") { "launch" } else { "script" };
    let order: Vec<&str> = calls.iter().filter(|call| call.env.get("T3_SSH_AUTH_SECRET").is_none_or(|secret| secret == "hunter2")).map(kind).collect();
    assert_eq!((order.first().copied(), order.last().copied()), (Some("resolve"), Some("script")), "{order:?}");
    assert_eq!(launches(&remote), 3, "refused, refused with the wrong password, accepted");
    let tunnel = calls.iter().find(|call| call.has("-N")).unwrap();
    assert!(tunnel.args.iter().any(|argument| argument.ends_with(":127.0.0.1:3773")), "{:?}", tunnel.args);
    // What is on disk is the host that was added, as typed into the page and not as ssh resolved it.
    let saved = std::fs::read_to_string(&remote.saved).unwrap();
    assert!(saved.contains("\"alias\": \"devbox\""), "{saved}");
    wait_for_rendered_text(&mut dom, "Connected").await;
    until_enabled(&mut dom, "Disconnect devbox").await;

    // Back to this computer's server: the tunnel stays open.
    click_when_enabled(&mut dom, "Use this computer").await;
    until(&mut dom, "the window on this computer", |_| destination(state).as_deref() == Some("diff-ui") && state.peek().status == ConnectionStatus::Connected).await;
    wait_for_rendered_text(&mut dom, "Tunnel open").await;
    assert_eq!(remote.stops.load(Ordering::SeqCst), 0);

    // Disconnecting stops the server the app started there.
    click_when_enabled(&mut dom, "Disconnect devbox").await;
    wait_for_rendered_text(&mut dom, "devbox was disconnected.").await;
    until_enabled(&mut dom, "Remove devbox").await;
    assert_eq!(remote.stops.load(Ordering::SeqCst), 1);
    assert!(state.peek().ssh.active.is_none());
    assert_eq!(state.error().peek().clone(), None);
    finish(fixture, remote, dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn cancelling_the_password_prompt_says_so_and_changes_nothing() {
    let fixture = Fixture::start(false).await;
    let remote = remote(Some("never"), "Host devbox\n").await;
    let (mut dom, state) = page(&fixture, &remote).await;
    input_control(&mut dom, "SSH host or alias", "devbox");
    click_when_enabled(&mut dom, "Add environment").await;
    let queue = remote.ssh.prompts().watch();
    until(&mut dom, "the question", |_| queue.borrow().len() == 1).await;
    let asked = queue.borrow()[0].clone();
    until(&mut dom, "the prompt", |dom| control_attribute(dom, "SSH password", "data-request").and_then(|(_, id)| id).as_deref() == Some(asked.request_id.as_str())).await;
    click_control(&mut dom, "Cancel SSH authentication");
    wait_for_rendered_text(&mut dom, "SSH authentication cancelled for julius@devbox.").await;
    until(&mut dom, "the dialog gone", |dom| control(dom, "SSH password").is_none()).await;
    until_enabled(&mut dom, "Add environment").await;
    // Still on this computer's server, with nothing added, and nobody asked again.
    assert_eq!((destination(state).as_deref(), state.peek().ssh.active.is_none(), state.peek().ssh.saved.len()), (Some("diff-ui"), true, 0));
    assert!(remote.ssh.prompts().watch().borrow().is_empty());
    assert_eq!(launches(&remote), 1);
    assert!(!remote.saved.exists());
    finish(fixture, remote, dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_typed_host_connects_a_bad_port_is_refused_before_ssh_runs_and_a_saved_host_is_offered_again() {
    let fixture = Fixture::start(false).await;
    let remote = remote(None, "").await;
    // Hosts added before are offered when the page opens, whether or not the host is in the ssh configuration.
    std::fs::write(&remote.saved, r#"[{"alias":"old-box","hostname":"old.example.com","username":null,"port":null}]"#).unwrap();
    let (mut dom, state) = page(&fixture, &remote).await;
    until_enabled(&mut dom, "Connect old-box").await;
    until_enabled(&mut dom, "Remove old-box").await;

    input_control(&mut dom, "SSH host or alias", "julius@box.example.com:2200");
    input_control(&mut dom, "SSH port", "70000");
    click_when_enabled(&mut dom, "Add environment").await;
    wait_for_rendered_text(&mut dom, "SSH port must be between 1 and 65535.").await;
    assert!(remote.spawner.recorded().is_empty(), "nothing ran");

    input_control(&mut dom, "SSH port", "");
    click_when_enabled(&mut dom, "Add environment").await;
    wait_for_rendered_text(&mut dom, "is ready over an SSH-managed tunnel.").await;
    until(&mut dom, "the window on the remote environment", |_| destination(state).as_deref() == Some("remote-ssh") && state.peek().status == ConnectionStatus::Connected).await;
    let resolved = remote.spawner.recorded().into_iter().find(|call| call.has("-G")).unwrap();
    assert!(resolved.args.iter().any(|argument| argument == "box.example.com"), "{:?}", resolved.args);
    // The form is empty again, and both hosts are in the list.
    until(&mut dom, "the form emptied", |dom| control(dom, "SSH host or alias").is_some_and(|(_, value)| value.as_deref() == Some(""))).await;
    until_enabled(&mut dom, "Disconnect box.example.com").await;
    until_enabled(&mut dom, "Connect old-box").await;
    assert_eq!(t3_desktop::ssh::load_saved(&remote.saved).len(), 2);
    drop(remote.home);
    fixture.finish(dom).await;
    remote.host.finish_server().await;
}
