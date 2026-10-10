//! Importing Claude Code history and tracked clones through the mounted UI and the real native
//! server: a scan of an isolated Claude home, the grouping and default selection, a partial
//! import's warning and a full import's landing; a clone that stalls on a loopback listener shows
//! its progress in a notice, on the project's form and in the sidebar, and cancels and retries.
//! Waits are on text or controls that only the awaited step produces.
use super::diff_tests::{Fixture, Harness, rendered_text, until, wait_for_rendered_text};
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::keys_tests::{key_on, open_palette};
use crate::runtime::transport_tests::{change_control, click_control, control, control_attribute, input_control};

const SCOPES: [AuthEnvironmentScope; 4] = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::SourceControlWrite, AuthEnvironmentScope::FilesystemRead];
const READER: [AuthEnvironmentScope; 1] = [AuthEnvironmentScope::OrchestrationRead];

fn transcript(cwd: &str, session: &str, text: &str) -> String {
    let stamp = (chrono::Utc::now() - chrono::Duration::minutes(10)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    [
        json!({"type":"summary","cwd":cwd,"sessionId":session}),
        json!({"type":"user","cwd":cwd,"sessionId":session,"timestamp":stamp,"message":{"role":"user","content":text}}),
        json!({"type":"assistant","cwd":cwd,"sessionId":session,"timestamp":stamp,"message":{"role":"assistant","model":"claude-sonnet-4-6","content":[{"type":"text","text":"Done."}]}}),
    ]
    .iter()
    .map(|line| line.to_string() + "\n")
    .collect()
}
/// Two directories outside the fixture's own home (the server leaves its own home out of a scan),
/// and the Claude history that names them.
fn history() -> (tempfile::TempDir, std::path::PathBuf, Vec<(&'static str, String)>) {
    let work = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(work.path()).unwrap();
    std::fs::create_dir_all(root.join("proj/.git")).unwrap();
    std::fs::write(root.join("proj/.git/config"), "[remote \"origin\"]\n\turl = git@github.com:acme/widgets.git\n").unwrap();
    std::fs::create_dir_all(root.join("solo")).unwrap();
    let (proj, solo) = (root.join("proj").display().to_string(), root.join("solo").display().to_string());
    let files = vec![
        ("claude-home/projects/-proj/one.jsonl", transcript(&proj, "11111111-2222-4333-8444-555555555555", "Add a widget")),
        ("claude-home/projects/-proj/two.jsonl", transcript(&proj, "22222222-2222-4333-8444-555555555555", "Fix the widget")),
        ("claude-home/projects/-proj/bad.jsonl", transcript(&proj, "not-a-uuid", "Unresumable")),
        ("claude-home/projects/-solo/solo.jsonl", transcript(&solo, "33333333-2222-4333-8444-555555555555", "Alone")),
    ];
    (work, root, files)
}
async fn open(fixture: &Fixture, scopes: &[AuthEnvironmentScope]) -> (VirtualDom, Harness, Store<UiModel>) {
    let (mut dom, props) = fixture.client(Gate::default(), scopes);
    let state = props.state.borrow().unwrap();
    until(&mut dom, "the shell", |_| state.peek().shell.synchronized).await;
    until(&mut dom, "the configuration", |_| state.peek().typed_config.is_some()).await;
    (dom, props, state)
}
fn disabled(dom: &VirtualDom, label: &str) -> bool {
    control_attribute(dom, label, "disabled").and_then(|(_, value)| value).as_deref() == Some("true")
}
fn projects(state: Store<UiModel>) -> Vec<(String, String)> {
    state.peek().shell.snapshot.as_ref().map(|snapshot| snapshot.projects.iter().map(|project| (project.title.to_string(), project.workspace_root.to_string())).collect()).unwrap_or_default()
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_scan_is_grouped_selected_and_imported_with_a_warning_for_what_was_skipped_and_a_landing_for_what_was_not() {
    let (_work, work_root, files) = history();
    let fixture = Fixture::start_claude(&[], &files).await;
    let (mut dom, _props, state) = open(&fixture, &SCOPES).await;
    click_control(&mut dom, "Import projects");
    wait_for_rendered_text(&mut dom, "Choose your projects").await;
    wait_for_rendered_text(&mut dom, "acme/widgets").await;
    let text = rendered_text(&dom);
    // The repository is first and chosen; the plain folder is folded away and not.
    assert!(text.contains("1 of 2 selected") && text.contains("Other folders") && text.contains("1 folder"), "{text}");
    assert!(text.contains("Claude Code") && text.contains("3 \u{b7} now"), "sources, thread count and age: {text}");
    assert!(!text.contains("Already a project"));
    assert!(control(&dom, "Select acme/widgets").is_some());

    // Importing a project makes it and brings in the threads that can resume; the rest are counted.
    click_control(&mut dom, "Import 1 project");
    assert!(disabled(&dom, "Importing\u{2026}"), "one run at a time");
    wait_for_rendered_text(&mut dom, "Imported 2 threads. 1 thread could not be imported.").await;
    assert!(projects(state).iter().any(|(title, root)| title == "proj" && root.ends_with("/proj")));
    until(&mut dom, "the imported threads", |_| state.peek().shell.snapshot.as_ref().is_some_and(|snapshot| snapshot.threads.len() == 2)).await;
    assert!(control(&dom, "Select acme/widgets").is_some(), "the dialog stays open on a partial import");

    // Choosing only the plain folder imports it completely: the dialog closes and the project is where the person lands.
    click_control(&mut dom, "Select none");
    assert!(rendered_text(&dom).contains("0 of 2 selected"));
    click_control(&mut dom, "Expand other folders");
    change_control(&mut dom, &format!("Select {}", work_root.join("solo").display()), "true");
    click_control(&mut dom, "Import 1 project");
    until(&mut dom, "the dialog to close", |dom| control(dom, "Do not import projects").is_none()).await;
    assert!(rendered_text(&dom).contains("Imported 1 thread"));
    until(&mut dom, "the solo project", |_| projects(state).iter().any(|(title, _)| title == "solo")).await;
    assert_eq!(state.selected_project().peek().is_some(), true, "the person lands in the project");
    assert!(state.active_thread().peek().is_none());

    // A project that exists is marked on the next scan.
    click_control(&mut dom, "Import projects");
    wait_for_rendered_text(&mut dom, "Already a project").await;
    key_on(&mut dom, "Choose projects to import", Key::Escape);
    assert!(control(&dom, "Do not import projects").is_none(), "Escape closes the dialog");
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_connection_that_cannot_operate_cannot_import() {
    let (_work, _root, files) = history();
    let fixture = Fixture::start_claude(&[], &files).await;
    let (mut dom, _props, _state) = open(&fixture, &READER).await;
    wait_for_rendered_text(&mut dom, "Import projects").await;
    assert!(disabled(&dom, "Import projects"), "the sidebar button needs the grant");
    // The palette does not offer it either.
    open_palette(&mut dom).await;
    assert!(control(&dom, "Import projects").is_none() || disabled(&dom, "Import projects"));
    fixture.finish(dom).await;
}

/// A loopback `git://` listener that accepts and never answers, so a clone from it stays where
/// git starts, and its address.
fn stalled_remote() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming().flatten() {
            held.push(stream);
        }
    });
    format!("git://{address}/repo.git")
}
fn start_clone(dom: &mut VirtualDom, props: &Harness, state: Store<UiModel>, project: &str, remote: &str, destination: &std::path::Path) {
    let input = t3_client::project_clone::start_input(project, "repo", "2026-10-09T12:00:00.000Z", remote, &destination.display().to_string());
    dom.in_scope(ScopeId::APP, || request(&props.transport, state, "projectClone.start", input, RequestKind::Unary)).expect("the clone is started");
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_clone_in_flight_shows_its_progress_where_the_person_is_and_can_be_cancelled_and_retried() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props, state) = open(&fixture, &SCOPES).await;
    let destination = fixture.workspace.parent().unwrap().join("clones/repo");
    let remote = stalled_remote();
    start_clone(&mut dom, &props, state, "cloned", &remote, &destination);
    // The notice, the sidebar and the project's form all say it is cloning, from the one stream.
    wait_for_rendered_text(&mut dom, "Cloning repo").await;
    wait_for_rendered_text(&mut dom, "Cloning \u{b7} Connecting").await;
    until(&mut dom, "the project", |_| projects(state).iter().any(|(title, _)| title == "repo")).await;
    dom.in_scope(ScopeId::APP, || new_thread(&props.transport, state, Some("cloned".into())));
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    until(&mut dom, "the form", |dom| control(dom, "Cancel clone").is_some()).await;
    assert!(!rendered_text(&dom).contains("Cloning repo\u{200b}"), "the notice steps aside on the form that shows the same");
    let title = |dom: &VirtualDom| control_attribute(dom, "Create new thread", "title").and_then(|(_, value)| value);
    assert_eq!(title(&dom).as_deref(), Some("Cloning repository"));
    assert!(disabled(&dom, "Create new thread"), "a project being cloned starts no thread");

    // Cancelling is asked once and answered: the clone ends cancelled and the project stays.
    click_control(&mut dom, "Cancel clone");
    assert!(disabled(&dom, "Cancel clone"), "off from the click until the server answers");
    wait_for_rendered_text(&mut dom, "Cancelled cloning repo").await;
    assert_eq!(title(&dom).as_deref(), Some("Repository not cloned"));
    assert!(control(&dom, "Remove project").is_some());
    until(&mut dom, "the server's answer to the cancel", |dom| !disabled(dom, "Retry clone")).await;
    assert!(rendered_text(&dom).contains("Clone cancelled"), "the sidebar says so too");

    // Retry starts the same clone again.
    click_control(&mut dom, "Retry clone");
    // The stream's word that the clone runs again can reach the page before the answer to the retry does,
    // and the control stays off until that answer: a person's click on it would do nothing either.
    until(&mut dom, "the clone running again, with the retry answered", |dom| control(dom, "Cancel clone").is_some() && !disabled(dom, "Cancel clone")).await;
    click_control(&mut dom, "Cancel clone");
    wait_for_rendered_text(&mut dom, "Cancelled cloning repo").await;
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_finished_clone_opens_its_project_and_allows_threads() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props, state) = open(&fixture, &SCOPES).await;
    let base = std::fs::canonicalize(fixture.directory.path()).unwrap();
    let work = base.join("seed-work");
    std::fs::create_dir_all(&work).unwrap();
    crate::runtime::diff_tests::git(&work, &["init", "-q"]);
    std::fs::write(work.join("README.md"), "hello\n").unwrap();
    crate::runtime::diff_tests::git(&work, &["add", "."]);
    crate::runtime::diff_tests::git(&work, &["commit", "-q", "-m", "init"]);
    let bare = base.join("origin.git");
    crate::runtime::diff_tests::git(&base, &["clone", "-q", "--bare", work.to_str().unwrap(), bare.to_str().unwrap()]);
    let destination = base.join("clones/landed");
    start_clone(&mut dom, &props, state, "landed", bare.to_str().unwrap(), &destination);
    wait_for_rendered_text(&mut dom, "Cloned landed").await;
    assert_eq!(std::fs::read_to_string(destination.join("README.md")).unwrap(), "hello\n");
    click_control(&mut dom, "Open project landed");
    assert_eq!(state.selected_project().peek().as_deref(), Some("landed"));
    until(&mut dom, "the form without a clone", |dom| control(dom, "Create new thread").is_some()).await;
    assert!(control(&dom, "Cancel clone").is_none() && control(&dom, "Retry clone").is_none());
    assert_ne!(control_attribute(&dom, "Create new thread", "title").and_then(|(_, value)| value).as_deref(), Some("Cloning repository"));
    fixture.finish(dom).await;
}
