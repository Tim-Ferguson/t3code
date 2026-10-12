//! Reading and acting on every connected environment at once, against two real in-process servers: the sidebar's rows, the search, the
//! command palette and the notifications span them, opening a row of the other environment moves the window to it, and what is done
//! to such a row is written to its own server. The window is on one environment; the other is parked and keeps receiving.
use super::diff_tests::until;
use super::environments_tests::{App, Remote, mutate, open_titles, parked_titles, two};
use super::keys_tests::{PALETTE, key_on, open_palette, poll_until};
use super::transport_tests::{click_control, control, drive_future, input_control, rendered_text};
use super::*;

/// A thread on the environment's own connection, whichever environment the window is on.
fn create_thread(dom: &mut VirtualDom, props: &App, state: Store<UiModel>, environment: &EnvironmentId, project: &str, id: &str, title: &str) {
    let link = props.transport.link_of(environment).expect("a connected environment");
    dom.in_scope(ScopeId::APP, || {
        command(&link, state, "thread.create", json!({"createdBy":"user","creationSource":"web","threadId":id,"projectId":project,"title":title,"modelSelection":{"instanceId":"codex","model":"test-provider-model"},"runtimeMode":"approval-required","interactionMode":"default","branch":null,"worktreePath":null}))
    })
    .expect("accepted");
}
/// The thread as the environment's own shell has it, whether the environment is open or parked.
fn thread_in(state: Store<UiModel>, environment: &EnvironmentId, id: &str) -> Option<t3_contracts::ThreadShell> {
    let model = state.peek();
    let shell = if model.destination.as_ref() == Some(environment) { &model.shell } else { &model.parked.get(environment)?.shell };
    shell.snapshot.as_ref()?.threads.iter().find(|thread| thread.id.as_str() == id).cloned()
}

/// A directory that is a checkout of `github.com/acme/widget`, so the server resolves one repository identity for every such directory.
fn checkout(name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let directory = tempfile::Builder::new().prefix(&format!("t3code-rust-port-test-fleet-{name}-")).tempdir().unwrap();
    let root = std::fs::canonicalize(directory.path()).unwrap();
    for arguments in [vec!["init", "-q"], vec!["remote", "add", "origin", "https://github.com/acme/widget.git"]] {
        assert!(std::process::Command::new("git").args(&arguments).current_dir(&root).env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_SYSTEM", "/dev/null").status().unwrap().success());
    }
    (directory, root)
}
/// The environments' shared repository as a project on each, the window on the first; waits for both identities.
async fn widgets(a: &Remote, b: &Remote) -> (VirtualDom, App, Store<UiModel>, [tempfile::TempDir; 2]) {
    let (mut dom, props, state) = two(a, b).await;
    let ((first, first_root), (second, second_root)) = (checkout("a"), checkout("b"));
    let create = |id: &str, root: &std::path::Path| json!({"type":"project.create","commandId":format!("create-{id}"),"projectId":id,"title":"Widget","workspaceRoot":root.to_string_lossy()});
    mutate(&mut dom, &props, state, &a.id, create("widget-a", &first_root));
    mutate(&mut dom, &props, state, &b.id, create("widget-b", &second_root));
    let identified = |environment: &EnvironmentId, id: &str| {
        let model = state.peek();
        let shell = if model.destination.as_ref() == Some(environment) { &model.shell } else { &model.parked[environment].shell };
        shell.snapshot.as_ref().is_some_and(|snapshot| snapshot.projects.iter().any(|project| project.id.as_str() == id && project.extra.get("repositoryIdentity").is_some_and(|identity| identity.is_object())))
    };
    let (a_id, b_id) = (a.id.clone(), b.id.clone());
    until(&mut dom, "both identities", |_| identified(&a_id, "widget-a") && identified(&b_id, "widget-b")).await;
    let b_id = b.id.clone();
    until(&mut dom, "the second's configuration", |_| state.peek().parked.get(&b_id).is_some_and(|slice| slice.typed_config.is_some())).await;
    until(&mut dom, "the first's configuration", |_| state.peek().typed_config.is_some()).await;
    (dom, props, state, [first, second])
}

/// Both environments with a project and a thread each, the window on the first. Waits until both shells have them and the parked
/// environment's configuration (its capabilities) has arrived.
async fn seeded(a: &Remote, b: &Remote) -> (VirtualDom, App, Store<UiModel>) {
    let (mut dom, props, state) = two(a, b).await;
    mutate(&mut dom, &props, state, &a.id, a.project("project-a", "Alpha project"));
    mutate(&mut dom, &props, state, &b.id, b.project("project-b", "Beta project"));
    until(&mut dom, "both projects", |_| open_titles(state) == ["Alpha project"] && parked_titles(state, &b.id) == ["Beta project"]).await;
    create_thread(&mut dom, &props, state, &a.id, "project-a", "thread-a", "Alpha thread");
    create_thread(&mut dom, &props, state, &b.id, "project-b", "thread-b", "Beta thread");
    until(&mut dom, "both threads", |_| thread_in(state, &a.id, "thread-a").is_some() && thread_in(state, &b.id, "thread-b").is_some()).await;
    let b_id = b.id.clone();
    until(&mut dom, "the second's configuration", |_| state.peek().parked.get(&b_id).is_some_and(|slice| slice.typed_config.is_some())).await;
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    (dom, props, state)
}

#[tokio::test(flavor = "current_thread")]
async fn the_sidebar_lists_every_environments_threads_and_opening_one_moves_the_window_to_its_environment() {
    let (a, b) = (Remote::start("env-a", "Alpha host").await, Remote::start("env-b", "Beta host").await);
    let (mut dom, _props, state) = seeded(&a, &b).await;

    // Both environments' projects and threads are rows of the one sidebar; the parked environment's rows are drawn from its own shell.
    until(&mut dom, "both environments' rows", |dom| control(dom, "Thread Alpha thread").is_some() && control(dom, "Thread Beta thread").is_some()).await;
    assert_eq!(state.peek().destination.as_ref(), Some(&a.id));
    assert!(control(&dom, "Switch to Alpha host").is_some(), "{}", rendered_text(&dom));

    // The row of the other environment opens that environment's thread: the window moves, and the thread follows its own server.
    click_control(&mut dom, "Thread Beta thread");
    assert_eq!((state.peek().destination.as_ref(), state.peek().active_thread.as_deref()), (Some(&b.id), Some("thread-b")));
    until(&mut dom, "the thread to open", |_| state.peek().thread.projection.as_ref().is_some_and(|projection| projection["thread"]["id"] == "thread-b")).await;
    assert_eq!(open_titles(state), ["Beta project"]);
    assert!(state.peek().parked.contains_key(&a.id), "the first environment stays connected, parked");
    // The row that is open is marked, and only it.
    assert_eq!(super::transport_tests::control_attribute(&dom, "Thread Beta thread", "aria-current").and_then(|(_, value)| value).as_deref(), Some("true"));
    assert_eq!(super::transport_tests::control_attribute(&dom, "Thread Alpha thread", "aria-current").and_then(|(_, value)| value).as_deref(), Some("false"));

    // And back, from the row of the environment that is now parked.
    click_control(&mut dom, "Thread Alpha thread");
    assert_eq!((state.peek().destination.as_ref(), state.peek().active_thread.as_deref()), (Some(&a.id), Some("thread-a")));
    until(&mut dom, "the first thread", |_| state.peek().thread.projection.as_ref().is_some_and(|projection| projection["thread"]["id"] == "thread-a")).await;
    drop(dom);
    a.server.stop().await;
    b.server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn what_is_done_to_a_row_of_the_other_environment_is_written_to_that_environments_server_and_the_window_stays() {
    use dioxus::prelude::Key;
    let (a, b) = (Remote::start("env-a", "Alpha host").await, Remote::start("env-b", "Beta host").await);
    let (mut dom, _props, state) = seeded(&a, &b).await;
    until(&mut dom, "the other environment's row", |dom| control(dom, "Thread Beta thread").is_some()).await;

    key_on(&mut dom, "Thread Beta thread", Key::ContextMenu);
    until(&mut dom, "the menu", |dom| control(dom, "Thread actions").is_some()).await;
    click_control(&mut dom, "Pin thread");
    until(&mut dom, "the pin on the second server", |_| thread_in(state, &b.id, "thread-b").is_some_and(|thread| t3_client::sidebar::is_pinned(&thread))).await;
    assert!(!thread_in(state, &a.id, "thread-a").is_some_and(|thread| t3_client::sidebar::is_pinned(&thread)), "the first server was not asked");
    assert_eq!((state.peek().destination.as_ref(), state.peek().active_thread.as_deref()), (Some(&a.id), None), "writing to a row does not move the window");
    assert_eq!(state.peek().error, None);
    // The pinned row shows what the second server answered, and the row is no longer busy.
    until(&mut dom, "the row's answer", |dom| super::transport_tests::control_attribute(dom, "Thread Beta thread", "aria-busy").and_then(|(_, value)| value).as_deref() == Some("false")).await;

    // An action that needs the window's own state (a confirmation) is done on the environment the thread is in.
    key_on(&mut dom, "Thread Beta thread", Key::ContextMenu);
    until(&mut dom, "the menu again", |dom| control(dom, "Thread actions").is_some()).await;
    click_control(&mut dom, "Unpin thread");
    until(&mut dom, "the unpin on the second server", |_| thread_in(state, &b.id, "thread-b").is_some_and(|thread| !t3_client::sidebar::is_pinned(&thread))).await;
    assert_eq!(state.peek().destination.as_ref(), Some(&b.id), "unpinning asks the window's own dialog, so the window moved");
    drop(dom);
    a.server.stop().await;
    b.server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn two_checkouts_of_one_repository_on_two_environments_are_one_row_of_threads_that_cannot_be_dragged() {
    if !std::process::Command::new("git").arg("--version").status().is_ok_and(|status| status.success()) {
        return;
    }
    let (a, b) = (Remote::start("env-a", "Alpha host").await, Remote::start("env-b", "Beta host").await);
    let (mut dom, props, state, _directories) = widgets(&a, &b).await;
    create_thread(&mut dom, &props, state, &a.id, "widget-a", "thread-a", "Alpha thread");
    create_thread(&mut dom, &props, state, &b.id, "widget-b", "thread-b", "Beta thread");
    until(&mut dom, "the grouped row with both threads", |dom| control(dom, "2 projects grouped").is_some() && control(dom, "Thread Alpha thread").is_some() && control(dom, "Thread Beta thread").is_some()).await;
    assert_eq!(crate::sidebar_groups::rows(state).len(), 1, "one row for the repository on both machines");
    let row = crate::sidebar_groups::rows(state).remove(0);
    assert_eq!(row.scope.refs, vec![("env-a".to_owned(), "widget-a".to_owned()), ("env-b".to_owned(), "widget-b".to_owned())]);
    assert_eq!(row.scope.environment(), None, "the row spans two environments");

    // The row's threads cannot be dragged: an order key written for one server would be ranked against the other's.
    let thread = thread_in(state, &a.id, "thread-a").unwrap();
    let view = |arrangeable: bool| crate::sidebar_rows::view(&a.id, true, arrangeable, &thread, false, None, crate::fleet::caps(state, &a.id), true, chrono::Utc::now(), &Default::default());
    assert!(view(row.scope.environment().is_some()).drops.is_empty() && !view(false).arrangeable);
    assert!(!view(true).drops.is_empty() && view(true).arrangeable, "a row of one environment can be dragged");
    // The group is told apart by where each directory is: the override of one directory is keyed by its environment.
    assert_eq!(crate::sidebar_groups::override_key("env-b", "/tmp/x"), "env-b:/tmp/x");
    drop(dom);
    a.server.stop().await;
    b.server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn search_and_the_palette_find_threads_of_both_environments_and_open_them_there() {
    let (a, b) = (Remote::start("env-a", "Alpha host").await, Remote::start("env-b", "Beta host").await);
    let (mut dom, _props, state) = seeded(&a, &b).await;
    until(&mut dom, "the rows", |dom| control(dom, "Thread Beta thread").is_some()).await;

    // The sidebar's search matches titles of every environment as it is typed, and a result opens in its own environment.
    input_control(&mut dom, "Search threads", "Beta");
    until(&mut dom, "the result", |dom| rendered_text(dom).contains("Beta project \u{b7} Beta host")).await;
    assert!(control(&dom, "Alpha thread").is_none(), "only the match is listed");
    let (_, listed) = super::transport_tests::control_attribute(&dom, "Beta thread", "aria-selected").expect("the result");
    assert_eq!(listed.as_deref(), Some("true"));
    // The environment is named beside the project while more than one is connected.
    assert!(rendered_text(&dom).contains("Beta project \u{b7} Beta host"), "{}", rendered_text(&dom));
    click_control(&mut dom, "Beta thread");
    assert_eq!((state.peek().destination.as_ref(), state.peek().active_thread.as_deref()), (Some(&b.id), Some("thread-b")));
    until(&mut dom, "the thread", |_| state.peek().thread.projection.as_ref().is_some_and(|projection| projection["thread"]["id"] == "thread-b")).await;

    // The palette lists the threads of both environments, and choosing one of the other environment moves the window back.
    open_palette(&mut dom).await;
    input_control(&mut dom, PALETTE, "Alpha thread");
    until(&mut dom, "the palette result", |dom| control(dom, "Alpha thread").is_some()).await;
    click_control(&mut dom, "Alpha thread");
    assert_eq!((state.peek().destination.as_ref(), state.peek().active_thread.as_deref()), (Some(&a.id), Some("thread-a")));
    drop(dom);
    a.server.stop().await;
    b.server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_thread_of_a_parked_environment_that_fails_is_announced_and_the_announcement_opens_it_there() {
    let (a, b) = (Remote::start("env-a", "Alpha host").await, Remote::start("env-b", "Beta host").await);
    let (mut dom, _props, mut state) = seeded(&a, &b).await;
    state.client_settings().write().in_app_notifications_enabled = true;
    // The first look at a thread only records it, so the environment's existing threads announce nothing.
    super::diff_tests::pump(&mut dom).await;
    assert!(state.peek().notices.is_empty());

    // The second server's thread fails (the fixture has no provider to run a turn, so the shell the server would send is written
    // into the environment's own state, which is what its stream does).
    {
        let mut parked = state.parked();
        let mut parked = parked.write();
        let thread = parked.get_mut(&b.id).and_then(|slice| slice.shell.snapshot.as_mut()).and_then(|snapshot| snapshot.threads.iter_mut().find(|thread| thread.id.as_str() == "thread-b")).expect("the parked thread");
        let mut value = serde_json::to_value(&*thread).unwrap();
        value["status"] = json!("failed");
        value["latestRunId"] = json!("run-1");
        *thread = serde_json::from_value(value).unwrap();
    }
    until(&mut dom, "the announcement", |_| state.peek().notices.iter().any(|notice| notice.title == "Thread failed")).await;
    assert_eq!(state.peek().destination.as_ref(), Some(&a.id), "announcing does not move the window");
    click_control(&mut dom, "Open thread");
    assert_eq!((state.peek().destination.as_ref(), state.peek().active_thread.as_deref()), (Some(&b.id), Some("thread-b")));
    drop(dom);
    a.server.stop().await;
    b.server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn the_new_thread_form_offers_every_machine_the_project_is_on_and_takes_what_was_written_there() {
    use super::transport_tests::change_control;
    let (a, b) = (Remote::start("env-a", "Alpha host").await, Remote::start("env-b", "Beta host").await);
    let (mut dom, props, state, _directories) = widgets(&a, &b).await;

    // The form on the first machine's checkout offers both machines, and a message is written.
    dom.in_scope(ScopeId::APP, || new_thread(&props.transport, state, Some("widget-a".into())));
    until(&mut dom, "the form", |dom| control(dom, "Run on").is_some() && control(dom, "First message").is_some()).await;
    assert_eq!(control(&dom, "Run on").and_then(|(_, value)| value).as_deref(), Some("env-a"));
    input_control(&mut dom, "First message", "Look at the build");
    let first_draft = state.peek().active_draft.clone().expect("the form's draft");
    assert_eq!(crate::new_thread::form_hosts(state).iter().map(|host| (host.environment.as_str(), host.project.as_str())).collect::<Vec<_>>(), [("env-a", "widget-a"), ("env-b", "widget-b")]);

    // Choosing the other machine moves the window and the form there, in that machine's checkout, with the message.
    change_control(&mut dom, "Run on", "env-b");
    assert_eq!((state.peek().destination.as_ref(), state.peek().selected_project.as_deref()), (Some(&b.id), Some("widget-b")));
    let second_draft = state.peek().active_draft.clone().expect("a draft on the second machine");
    until(&mut dom, "the message on the second machine", |dom| control(dom, "First message").and_then(|(_, value)| value).as_deref() == Some("Look at the build")).await;
    assert_eq!(control(&dom, "Run on").and_then(|(_, value)| value).as_deref(), Some("env-b"));
    assert_eq!(state.peek().new_thread_drafts.get(&(b.id.clone(), second_draft.clone())).map(String::as_str), Some("Look at the build"));
    // The draft left behind went with the move.
    let left = t3_client::draft_storage::DraftTarget::draft("env-a", first_draft.as_str());
    assert!(state.peek().draft_storage.document.borrow().draft(&left).is_none());
    assert_eq!(state.peek().parked[&a.id].active_draft, None);

    // `composer.cycleHost` steps to the next machine, wrapping, and brings the message along again.
    dom.in_scope(ScopeId::APP, || crate::new_thread::cycle_host(&props.transport, state));
    assert_eq!(state.peek().destination.as_ref(), Some(&a.id));
    until(&mut dom, "the message on the first machine", |dom| control(dom, "First message").and_then(|(_, value)| value).as_deref() == Some("Look at the build")).await;

    // Files were uploaded to the machine they were attached on, so the form keeps them there.
    let draft = state.peek().active_draft.clone().unwrap();
    let target = t3_client::draft_storage::DraftTarget::draft("env-a", draft.as_str());
    state.draft_attachments().write().insert(target, vec![t3_client::attachments::Attachment { id: "file".into(), kind: t3_client::attachments::Kind::File, name: "notes.txt".into(), mime_type: "text/plain".into(), size_bytes: 3, attachment_id: Some("upload".into()), upload: t3_client::attachments::Upload::Ready, bytes: None, preview: None }]);
    until(&mut dom, "the picker locked", |dom| super::transport_tests::control_attribute(dom, "Run on", "disabled").and_then(|(_, value)| value).is_some()).await;
    dom.in_scope(ScopeId::APP, || crate::new_thread::cycle_host(&props.transport, state));
    assert_eq!(state.peek().destination.as_ref(), Some(&a.id), "the machine does not change under the files");
    assert!(state.peek().notices.iter().any(|notice| notice.title == "Could not change machine"));
    drop(dom);
    a.server.stop().await;
    b.server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn an_environment_that_is_not_on_this_computer_signs_in_to_chatgpt_through_the_local_one_and_keeps_the_profile() {
    use crate::chatgpt_handoff::{self, Progress};
    let issuer = super::fake_issuer::Issuer::start().await;
    let (local, remote) = (Remote::start_signing("env-a", "Local host", &issuer.discovery_url()).await, Remote::start_signing("env-b", "Remote host", &issuer.discovery_url()).await);
    let (mut dom, props, state) = two(&local, &remote).await;
    // Both fixtures listen on loopback; the second stands for a server on another computer.
    chatgpt_handoff::REMOTE.with(|set| set.borrow_mut().insert(remote.id.clone()));
    let route = |environment: &EnvironmentId| chatgpt_handoff::route(&props.transport, state, environment);
    assert!(route(&local.id).is_none(), "an environment on this computer receives the redirect itself");
    let route = route(&remote.id).expect("the local environment signs in for the remote one");
    assert_eq!(route.remote_environment, remote.id);

    // Nothing is registered on the remote yet.
    let asked = props.transport.link_of(&remote.id).unwrap();
    let before = drive_future(&mut dom, "the registration", request_value(asked.clone(), state, "provider.chatgpt.reconnect-profile", json!({"instanceId":"codex","methodId":"chatgpt"}))).await.unwrap();
    assert_eq!(before, Value::Null);

    // The sign-in runs on the local environment and reports the page the person opens.
    let reports = Rc::new(RefCell::new(Vec::<Progress>::new()));
    dom.in_scope(ScopeId::APP, || {
        let (reports, route) = (reports.clone(), route.clone());
        spawn(async move { chatgpt_handoff::run(state, route, "codex".into(), "chatgpt".into(), "http://localhost:5733/settings/providers".into(), move |progress| reports.borrow_mut().push(progress)).await });
    });
    let page = |reports: &Rc<RefCell<Vec<Progress>>>| reports.borrow().iter().find_map(|progress| match progress {
        Progress::Auth(auth) => auth["authorizationUrl"].as_str().or(auth["interaction"]["url"].as_str()).map(str::to_owned),
        _ => None,
    });
    // The reports are plain state the page draws nothing from, so the wait polls the page rather than waiting for it to repaint.
    poll_until(&mut dom, "the sign-in page", |_| page(&reports).is_some()).await;
    // The person's browser goes to the issuer, which sends it back to the local environment's own listener with a code.
    let url = page(&reports).unwrap();
    let returned = drive_future(&mut dom, "the browser", async { reqwest::get(&url).await.unwrap().status().is_success() }).await;
    assert!(returned);
    poll_until(&mut dom, "the profile to be kept", |_| reports.borrow().iter().any(|progress| matches!(progress, Progress::Done | Progress::Failed(_)))).await;
    let seen = reports.borrow().clone();
    assert!(matches!(seen.last(), Some(Progress::Done)), "{seen:?}");
    assert!(seen.contains(&Progress::Importing));

    // OpenAI was told the connection is for the remote environment, not the local one that ran the sign-in.
    let request = issuer.authorized.lock().unwrap().last().cloned().unwrap();
    assert_eq!(request["ext_agent_host_id"], "urn:uuid:env-b");
    // The remote environment keeps the registration now; the local one, which only ran the sign-in, kept none.
    let after = drive_future(&mut dom, "the registration", request_value(asked, state, "provider.chatgpt.reconnect-profile", json!({"instanceId":"codex","methodId":"chatgpt"}))).await.unwrap();
    assert!(after["clientId"].is_string() && after["email"].is_string(), "{after}");
    let local_link = props.transport.link_of(&local.id).unwrap();
    let kept = drive_future(&mut dom, "the registration", request_value(local_link, state, "provider.chatgpt.reconnect-profile", json!({"instanceId":"codex","methodId":"chatgpt"}))).await.unwrap();
    assert_eq!(kept, Value::Null);

    // A profile the issuer does not vouch for is refused by the environment asked to keep it, with the reason.
    let forged = json!({"instanceId":"codex","profile":{"registration":{"clientId":"oaiapp_test","subject":"someone-else"},"credentials":{"clientId":"oaiapp_test","subject":"someone-else","accessToken":"a","refreshToken":"r"}}});
    let refused = drive_future(&mut dom, "the import", request_value(props.transport.link_of(&remote.id).unwrap(), state, "provider.chatgpt.import-profile", forged)).await;
    assert!(refused.is_err(), "{refused:?}");
    chatgpt_handoff::REMOTE.with(|set| set.borrow_mut().clear());
    drop(dom);
    local.server.stop().await;
    remote.server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn the_local_environment_receives_the_redirect_of_a_sign_in_that_the_remote_one_started() {
    use crate::chatgpt_handoff;
    let (local, remote) = (Remote::start_signing("env-a", "Local host", "http://127.0.0.1:9/unused").await, Remote::start("env-b", "Remote host").await);
    let (mut dom, props, state) = two(&local, &remote).await;
    chatgpt_handoff::REMOTE.with(|set| set.borrow_mut().insert(remote.id.clone()));
    let route = chatgpt_handoff::route(&props.transport, state, &remote.id).expect("a route through the local environment");

    // The sign-in page redirects to a loopback port; the local environment listens there for it.
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let sign_in = "0123456789abcdef0123456789abcdef";
    let page = format!("https://auth.openai.com/api/accounts/authorize?response_type=code&client_id=dynamic_agent_client&redirect_uri=http%3A%2F%2F127.0.0.1%3A{port}%2Fauth%2Fcallback&state={sign_in}&code_challenge=E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM&code_challenge_method=S256");
    let (received, listening) = (Rc::new(RefCell::new(None::<Result<String, String>>)), Rc::new(std::cell::Cell::new(false)));
    dom.in_scope(ScopeId::APP, || {
        let (received, listening, route, page) = (received.clone(), listening.clone(), route.clone(), page.clone());
        spawn(async move {
            let outcome = chatgpt_handoff::receive_redirect(state, &route, "codex", "flow-1", &page, "http://localhost:5733/settings/providers", move || listening.set(true)).await;
            *received.borrow_mut() = Some(outcome);
        });
    });
    // The environment says when its listener is up; the person's browser then comes back with a code.
    poll_until(&mut dom, "the listener", |_| listening.get() || received.borrow().is_some()).await;
    assert!(listening.get(), "{:?}", received.borrow());
    let returned = format!("http://127.0.0.1:{port}/auth/callback?code=abc&state={sign_in}");
    let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
    let answered = drive_future(&mut dom, "the browser", client.get(&returned).send()).await.unwrap().status();
    assert!(answered.is_redirection() || answered.is_success(), "{answered}");
    poll_until(&mut dom, "the redirect", |_| received.borrow().is_some()).await;
    assert_eq!(received.borrow().clone(), Some(Ok(returned)));
    chatgpt_handoff::REMOTE.with(|set| set.borrow_mut().clear());
    drop(dom);
    local.server.stop().await;
    remote.server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn the_wizard_sets_up_every_ticked_computer_in_turn_and_unticking_one_leaves_it_out() {
    use super::diff_tests::wait_for_rendered_text;
    let (a, b) = (Remote::start("env-a", "Alpha host").await, Remote::start("env-b", "Beta host").await);
    let (mut dom, _props, state) = two(&a, &b).await;
    dom.in_scope(ScopeId::APP, || crate::welcome::open(state));
    wait_for_rendered_text(&mut dom, "Connect your computers").await;
    until(&mut dom, "both computers", |dom| control(dom, "Alpha host").is_some() && control(dom, "Beta host").is_some() && rendered_text(dom).matches("Connected").count() >= 2).await;
    // Both are ticked, since both are connected; the choice is the person's to change.
    let ticked = |dom: &VirtualDom, label: &str| super::transport_tests::control_attribute(dom, label, "checked").and_then(|(_, value)| value).as_deref() == Some("true");
    assert!(ticked(&dom, "Alpha host") && ticked(&dom, "Beta host"), "{}", rendered_text(&dom));

    click_control(&mut dom, "Continue to agents");
    wait_for_rendered_text(&mut dom, "Connect your agents").await;
    assert_eq!(state.peek().welcome.queue, vec![a.id.clone(), b.id.clone()], "the computer the window is on is first");
    assert_eq!(state.peek().destination.as_ref(), Some(&a.id));
    assert!(rendered_text(&dom).contains("computer 1 of 2"), "{}", rendered_text(&dom));
    click_control(&mut dom, "Continue to projects");
    wait_for_rendered_text(&mut dom, "Choose your projects").await;
    until(&mut dom, "the scan to answer", |dom| control(dom, "Do not import projects").is_some()).await;
    click_control(&mut dom, "Do not import projects");

    // The window moved to the second computer for its agents, and the wizard is still open.
    wait_for_rendered_text(&mut dom, "computer 2 of 2").await;
    assert_eq!((state.peek().destination.as_ref(), state.peek().welcome.open), (Some(&b.id), true));
    click_control(&mut dom, "Continue to projects");
    until(&mut dom, "the second scan", |dom| control(dom, "Do not import projects").is_some()).await;
    click_control(&mut dom, "Do not import projects");
    until(&mut dom, "the wizard to close", |_| !state.peek().welcome.open).await;
    assert!(state.peek().client_settings.onboarding_completed_at.is_some());

    // Run again with the second computer unticked: only the first is set up, and the wizard ends after it.
    dom.in_scope(ScopeId::APP, || crate::welcome::open(state));
    wait_for_rendered_text(&mut dom, "Connect your computers").await;
    until(&mut dom, "the computers", |dom| control(dom, "Alpha host").is_some()).await;
    state.welcome().write().unselected.insert(a.id.clone());
    until(&mut dom, "the choice to show", |dom| !ticked(dom, "Alpha host") && ticked(dom, "Beta host")).await;
    click_control(&mut dom, "Continue to agents");
    wait_for_rendered_text(&mut dom, "Connect your agents").await;
    assert_eq!((state.peek().welcome.queue.clone(), state.peek().destination.as_ref()), (vec![b.id.clone()], Some(&b.id)));
    assert!(!rendered_text(&dom).contains("computer 1 of"), "a single computer is not counted");
    drop(dom);
    a.server.stop().await;
    b.server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn auto_balance_starts_the_form_on_the_machine_with_the_most_free_capacity_weighted_by_the_preferences() {
    use super::transport_tests::change_control;
    let (a, b) = (Remote::start_loaded("env-a", "Alpha host", 0.6).await, Remote::start_loaded("env-b", "Beta host", 0.05).await);
    let (mut dom, props, state, _directories) = widgets(&a, &b).await;
    dom.in_scope(ScopeId::APP, || new_thread(&props.transport, state, Some("widget-a".into())));
    until(&mut dom, "the form", |dom| control(dom, "Run on").is_some()).await;

    // Balancing is the person's to turn on, on the Connections page, once there are two machines.
    assert!(!rendered_text(&dom).contains("Auto balance"), "not offered while it is off");
    state.view().set(View::Connections);
    until(&mut dom, "the section", |dom| control(dom, "Automatically balance load").is_some()).await;
    assert!(super::transport_tests::control_attribute(&dom, "Beta host load preference", "disabled").and_then(|(_, value)| value).is_some(), "a machine's preference waits for the switch");
    change_control(&mut dom, "Automatically balance load", "true");
    until(&mut dom, "balancing on", |_| state.peek().client_settings.load_balancing_enabled).await;
    state.view().set(View::Chat);
    until(&mut dom, "the choice", |dom| rendered_text(dom).contains("Auto balance")).await;

    // The idle machine takes the thread: the window moves to it, and the person is told.
    change_control(&mut dom, "Run on", "auto");
    until(&mut dom, "the window to move", |_| state.peek().destination.as_ref() == Some(&b.id)).await;
    assert_eq!(state.peek().selected_project.as_deref(), Some("widget-b"));
    assert!(state.peek().notices.iter().any(|notice| notice.title == "Auto balance" && notice.detail.contains("Beta host")), "{:?}", state.peek().notices);

    // A machine set to "Manual only" is never chosen, however idle it is.
    state.view().set(View::Connections);
    until(&mut dom, "the section again", |dom| control(dom, "Beta host load preference").is_some()).await;
    change_control(&mut dom, "Beta host load preference", "0");
    until(&mut dom, "the preference", |_| state.peek().client_settings.load_balancing_weights.iter().any(|(id, weight)| id.as_str() == "env-b" && weight.0 == 0)).await;
    state.view().set(View::Chat);
    until(&mut dom, "the form on the second machine", |dom| control(dom, "Run on").is_some()).await;
    change_control(&mut dom, "Run on", "auto");
    until(&mut dom, "the window to move back", |_| state.peek().destination.as_ref() == Some(&a.id)).await;
    drop(dom);
    a.server.stop().await;
    b.server.stop().await;
}
