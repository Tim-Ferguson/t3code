//! The diff view through the mounted UI and the real native HTTP/WebSocket server, with real
//! turns in a temporary git repository so that real checkpoints exist: the changed-files card,
//! the turn and whole-thread diffs, whitespace, unavailable and failed states, and what happens
//! when the selection moves while a diff is loading. Waits are on text or controls that only
//! the awaited step produces.
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::transport_tests::{
    api_fixture, change_control, click_control, control, control_attribute, input_control,
    start_fixture,
};
use std::{cell::RefCell, path::{Path, PathBuf}, process::Command, rc::Rc, time::Duration};

/// The page's text without its own stylesheets, which a text search would otherwise match.
pub(crate) fn rendered_text(dom: &VirtualDom) -> String {
    crate::runtime::transport_tests::rendered_text(dom)
        .replace(include_str!("../assets/app.css"), "")
        .replace(include_str!("../assets/theme-tokens.css"), "")
}
pub(crate) async fn wait_for_rendered_text(dom: &mut VirtualDom, expected: &str) {
    until(dom, expected, |dom| rendered_text(dom).contains(expected)).await;
}

#[derive(Clone)]
pub(crate) struct Harness {
    pub(crate) state: Rc<RefCell<Option<Store<UiModel>>>>,
    pub(crate) transport: TransportHandle,
    address: String,
    credential: String,
    gate: Gate,
}
pub(crate) fn harness(props: Harness) -> Element {
    let state = use_store(UiModel::default);
    crate::themes::use_themes(state);
    use_context_provider(|| props.gate.clone());
    *props.state.borrow_mut() = Some(state);
    let transport = props.transport.clone();
    use_future(move || {
        let transport = transport.clone();
        let address = props.address.clone();
        let credential = props.credential.clone();
        async move { pair_and_connect(transport, state, address, credential).await }
    });
    rsx! {crate::Application {state,transport:props.transport}}
}

pub(crate) async fn until(dom: &mut VirtualDom, label: &str, ready: impl Fn(&VirtualDom) -> bool) {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if ready(dom) {
                return;
            }
            dom.wait_for_work().await;
            dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        }
    })
    .await
    .unwrap_or_else(|_| panic!("diff milestone {label} timed out; rendered: {}", rendered_text(dom)));
}

/// What a render-isolation test measures: how often the app, the conversation and the composer
/// were built, and how many events the server has delivered to the shell and to the thread.
pub(crate) type Repaints = ((usize, usize, usize), (u64, u64));
pub(crate) fn repaints(state: Store<UiModel>, probe: &crate::RenderProbe) -> Repaints {
    let shell = state.peek().shell.snapshot.as_ref().map_or(0, |snapshot| snapshot.snapshot_sequence);
    ((probe.application.get(), probe.timeline.get(), probe.composer.get()), (shell, state.peek().thread.sequence))
}
/// The server's last events of a turn can arrive while a test is already measuring, and each
/// rightly repaints what it changes. The UI under test must repaint no more than that: the shell
/// and the composer for a shell event, the conversation for a thread event.
pub(crate) fn assert_repaints_explained_by_events(before: Repaints, after: Repaints, what: &str) {
    let (shell_events, thread_events) = (after.1.0 - before.1.0, after.1.1 - before.1.1);
    let (app, timeline, composer) = ((after.0.0 - before.0.0) as u64, (after.0.1 - before.0.1) as u64, (after.0.2 - before.0.2) as u64);
    assert!(
        app <= shell_events && composer <= shell_events && timeline <= thread_events,
        "{what}: repainted app {app}, conversation {timeline}, composer {composer} for {shell_events} shell and {thread_events} thread events"
    );
}

pub(crate) fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(["-c", "user.email=test@test.com", "-c", "user.name=Test", "-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap()
}

pub(crate) struct Fixture {
    pub(crate) directory: tempfile::TempDir,
    pub(crate) workspace: PathBuf,
    auth: t3_server::auth::AuthService,
    pub(crate) address: String,
    /// Stops the server, the execution service and the settings owner (whose worker runs on the
    /// runtime's blocking pool, which a runtime cannot be dropped without), awaited or dropped.
    server: crate::runtime::fixture_server::Server,
}
impl Fixture {
    pub(crate) async fn start(git_repository: bool) -> Self {
        Self::start_with(git_repository, &[]).await
    }
    /// `environment` is extra provider-process environment, such as the fixture's legacy-history switch.
    pub(crate) async fn start_with(git_repository: bool, environment: &[(&str, &str)]) -> Self {
        Self::build(git_repository, environment, false).await
    }
    /// With a terminal service, which a project's setup script runs in. `/bin/sh` is its shell, and
    /// every script is a gate file away from finishing.
    pub(crate) async fn start_with_terminals(git_repository: bool) -> Self {
        Self::build(git_repository, &[], true).await
    }
    async fn build(git_repository: bool, environment: &[(&str, &str)], terminals: bool) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        let workspace = root.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        if git_repository {
            git(&workspace, &["init", "-q"]);
            std::fs::write(workspace.join("README.md"), "hello\n").unwrap();
            git(&workspace, &["add", "."]);
            git(&workspace, &["commit", "-q", "-m", "initial"]);
        }
        let mut api = api_fixture(&directory, "diff-ui");
        let binary = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../server/tests/fixtures/codex-provider.py")
            .canonicalize()
            .unwrap();
        let settings: t3_contracts::ServerSettings = serde_json::from_value(
            json!({"providerInstances":{"claudeAgent":{"driver":"claudeAgent","enabled":false},"codex":{"driver":"codex","config":{"binaryPath":binary,"homePath":root.join("codex-home")},"environment":environment.iter().map(|(name, value)| json!({"name":name,"value":value})).chain(std::iter::once(json!({"name":"CODEX_FIXTURE_WORKSPACE","value":workspace}))).collect::<Vec<_>>()}}}),
        )
        .unwrap();
        // The settings service the configuration stream needs, over the same settings.
        let settings_file = root.join("settings.json");
        std::fs::write(&settings_file, serde_json::to_vec(&settings).unwrap()).unwrap();
        let mut options = t3_server::server_settings::SettingsOptions::file(settings_file, t3_server::server_secret_store::ServerSecretStore::open(root.join("secrets")).unwrap());
        options.watch = false;
        let settings_service = t3_server::server_settings::SettingsService::start(options).await.unwrap();
        // The canonical path, so the keybindings file watcher (which reports real paths) matches it.
        let native = t3_server::config::NativeConfig::from_settings(
            settings,
            &root,
            &root,
            &api.environment,
            &api.auth.descriptor(),
        )
        .await
        .unwrap();
        let execution = t3_server::execution::ExecutionService::start(api.store.clone(), native.providers.clone());
        api.config = Some(native.snapshot);
        api.settings = Some(settings_service.clone());
        api.providers = Some(native.providers);
        if terminals {
            let mut options = t3_server::terminal_manager::TerminalManagerOptions::host(root.join("terminal-logs"), &t3_contracts::ServerSettings::default());
            options.shell = Some("/bin/sh".into());
            options.kill_grace = std::time::Duration::ZERO;
            api.terminals = Some(t3_server::terminal_manager::TerminalManager::new(options).await.unwrap());
            execution.workspace().set_completion_shell("sh");
        }
        api.execution = Some(execution);
        api.workspace = Some(t3_server::workspace_entries::WorkspaceEntries::new(root.clone(), workspace.clone()));
        let auth = api.auth.clone();
        let (address, server) = start_fixture(api).await;
        Self { directory, workspace, auth, address, server }
    }

    pub(crate) fn operator(&self, gate: Gate) -> (VirtualDom, Harness) {
        self.client(gate, &[AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate])
    }
    /// A client paired with exactly these scopes.
    pub(crate) fn client(&self, gate: Gate, scopes: &[AuthEnvironmentScope]) -> (VirtualDom, Harness) {
        let credential = self
            .auth
            .create_pairing_credential(
                scopes,
                chrono::Utc::now(),
                chrono::Duration::minutes(1),
            )
            .unwrap();
        let props = Harness {
            state: Rc::new(RefCell::new(None)),
            transport: TransportHandle::default(),
            address: self.address.clone(),
            credential,
            gate,
        };
        let mut dom = VirtualDom::new_with_props(harness, props.clone());
        dom.rebuild_in_place();
        (dom, props)
    }

    pub(crate) async fn finish(self, dom: VirtualDom) {
        drop(dom);
        self.server.await;
    }

    pub(crate) fn write(&self, name: &str, text: &str) {
        std::fs::write(self.workspace.join(name), text).unwrap();
    }
}

/// A project over the workspace with threads `thread-a` and `thread-b`.
pub(crate) async fn seed(dom: &mut VirtualDom, props: &Harness, workspace: &Path) -> Store<UiModel> {
    let state = props.state.borrow().unwrap();
    until(dom, "shell", |_| state.peek().shell.synchronized).await;
    dom.in_scope(ScopeId::APP, || request(&props.transport, state, "projects.mutate", json!({"type":"project.create","commandId":"project","projectId":"project","title":"Project","workspaceRoot":workspace}), RequestKind::Unary)).unwrap();
    until(dom, "project", |_| state.peek().shell.snapshot.as_ref().is_some_and(|s| !s.projects.is_empty())).await;
    for thread in ["thread-a", "thread-b"] {
        dom.in_scope(ScopeId::APP, || command(&props.transport, state, "thread.create", json!({"threadId":thread,"projectId":"project","title":thread,"modelSelection":{"instanceId":"codex","model":"fixture-model"},"runtimeMode":"full-access","interactionMode":"default","branch":null,"worktreePath":null}))).unwrap();
    }
    until(dom, "threads", |_| state.peek().shell.snapshot.as_ref().is_some_and(|s| s.threads.len() == 2)).await;
    state
}

pub(crate) async fn open_thread(dom: &mut VirtualDom, props: &Harness, state: Store<UiModel>, thread: &str) {
    dom.in_scope(ScopeId::APP, || select_thread(&props.transport, state, thread.into()));
    until(dom, "thread", |_| state.peek().thread.synchronized && state.peek().thread.projection.as_ref().unwrap()["thread"]["id"] == thread).await;
}

pub(crate) fn completed_runs(state: Store<UiModel>) -> usize {
    state.peek().thread.projection.as_ref().and_then(|p| p["runs"].as_array()).map_or(0, |runs| runs.iter().filter(|run| run["status"] == "completed").count())
}

/// Sends a message and waits for the turn to settle, which is when its checkpoint exists.
pub(crate) async fn turn(dom: &mut VirtualDom, props: &Harness, state: Store<UiModel>, number: usize) {
    input_control(dom, "Message", &format!("Do the work {number}"));
    dom.in_scope(ScopeId::APP, || send_message(&props.transport, state));
    until(dom, "turn", |_| completed_runs(state) == number).await;
}

/// Opens the panel from the header button, which lands on "Changes" (the original's default), and
/// moves it to the newest turn, which is what the checkpoint tests are about.
pub(crate) fn open_latest(dom: &mut VirtualDom) {
    click_control(dom, "Diff");
    change_control(dom, "Diff scope", "latest");
}

pub(crate) fn error(state: Store<UiModel>) -> Option<String> {
    state.error().peek().clone()
}

/// Turn 1 changes nothing; turn 2 edits README.md and adds hello.txt; turn 3 extends hello.txt
/// by two lines.
async fn three_turns(fixture: &Fixture, dom: &mut VirtualDom, props: &Harness, state: Store<UiModel>) {
    open_thread(dom, props, state, "thread-a").await;
    turn(dom, props, state, 1).await;
    fixture.write("README.md", "hello\nmore\n");
    fixture.write("hello.txt", "world\n");
    turn(dom, props, state, 2).await;
    fixture.write("hello.txt", "world\nagain\nand more\n");
    turn(dom, props, state, 3).await;
}

/// Dispatches a key press to the element with this accessible label.
fn press(dom: &mut VirtualDom, label: &str, key: Key) {
    dioxus_html::set_event_converter(Box::new(dioxus_html::SerializedHtmlEventConverter));
    let (element, _) = control(dom, label).unwrap_or_else(|| panic!("missing control {label}"));
    let data = dioxus_html::SerializedKeyboardData::new(
        key,
        Code::Escape,
        Location::Standard,
        false,
        Modifiers::empty(),
        false,
    );
    dom.runtime().handle_event(
        "keydown",
        dioxus::dioxus_core::Event::new(Rc::new(dioxus_html::PlatformEventData::new(Box::new(data))) as Rc<dyn std::any::Any>, true),
        element,
    );
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_completed_turn_lists_its_files_and_opens_its_diff_at_the_file_clicked() {
    let fixture = Fixture::start(true).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    turn(&mut dom, &props, state, 1).await;
    assert!(!rendered_text(&dom).contains("changed file"), "a turn that changed nothing has no card");
    assert!(control(&dom, "Diff scope").is_none(), "the panel starts closed");
    fixture.write("README.md", "hello\nmore\n");
    fixture.write("hello.txt", "world\n");
    turn(&mut dom, &props, state, 2).await;
    wait_for_rendered_text(&mut dom, "2 changed files").await;
    // The card names the files and carries their counts.
    assert!(control(&dom, "Show diff for README.md").is_some() && control(&dom, "Show diff for hello.txt").is_some());
    assert!(control(&dom, "Open diff").is_some());

    // Clicking a file opens that turn with that file expanded; the other stays collapsed.
    click_control(&mut dom, "Show diff for hello.txt");
    wait_for_rendered_text(&mut dom, "@@ -0,0 +1 @@").await;
    let text = rendered_text(&dom);
    assert!(text.contains("world") && !text.contains("@@ -1 +1,2 @@"), "{text}");
    assert!(control(&dom, "Collapse hello.txt").is_some() && control(&dom, "Expand README.md").is_some());
    assert_eq!(control(&dom, "Diff scope").unwrap().1.as_deref(), Some("latest"));
    assert_eq!(error(state), None);

    // Expanding the other file shows its hunk.
    click_control(&mut dom, "Expand README.md");
    assert!(rendered_text(&dom).contains("@@ -1 +1,2 @@"));
    // Collapse all, then expand all.
    click_control(&mut dom, "Collapse all files");
    assert!(control(&dom, "Expand README.md").is_some() && control(&dom, "Expand hello.txt").is_some());
    assert!(!rendered_text(&dom).contains("@@ -0,0 +1 @@"));
    click_control(&mut dom, "Expand all files");
    assert!(rendered_text(&dom).contains("@@ -0,0 +1 @@") && rendered_text(&dom).contains("@@ -1 +1,2 @@"));

    // The tree jumps to a file without changing the selection.
    click_control(&mut dom, "Show file tree");
    assert!(control(&dom, "Go to hello.txt").is_some());
    click_control(&mut dom, "Go to README.md");
    assert_eq!(control(&dom, "Diff scope").unwrap().1.as_deref(), Some("latest"));
    click_control(&mut dom, "Hide file tree");
    assert!(control(&dom, "Go to hello.txt").is_none());

    // Layout: split draws paired cells, stacked draws rows.
    click_control(&mut dom, "Split diff view");
    assert_eq!(state.client_settings().peek().diff_layout, t3_contracts::DiffLayout::Split);
    click_control(&mut dom, "Stacked diff view");
    assert_eq!(state.client_settings().peek().diff_layout, t3_contracts::DiffLayout::Stacked);

    // Escape in the panel closes it, like the close button and the header button do; the
    // card in the conversation opens it again.
    press(&mut dom, "Diff panel", Key::Escape);
    assert!(control(&dom, "Diff scope").is_none());
    click_control(&mut dom, "Open diff");
    assert!(control(&dom, "Diff scope").is_some());
    click_control(&mut dom, "Diff");
    assert!(control(&dom, "Diff scope").is_none(), "the header button closes what it opened");
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn turns_and_the_whole_thread_each_show_their_own_patch() {
    let fixture = Fixture::start(true).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    three_turns(&fixture, &mut dom, &props, state).await;
    wait_for_rendered_text(&mut dom, "1 changed file").await;

    // The header button opens the newest turn: turn 3 only touched hello.txt.
    open_latest(&mut dom);
    until(&mut dom, "latest turn", |dom| control(dom, "Expand all files").is_some()).await;
    assert!(control(&dom, "Expand hello.txt").is_some() && control(&dom, "Expand README.md").is_none());
    click_control(&mut dom, "Expand all files");
    assert!(rendered_text(&dom).contains("again") && rendered_text(&dom).contains("@@ -1 +1,3 @@"));
    assert!(!rendered_text(&dom).contains("@@ -1 +1,2 @@"));

    // An older turn: turn 2 changed both files, and turn 3's line is not in it.
    change_control(&mut dom, "Diff scope", &format!("turn:{}", run_id(state, 2)));
    until(&mut dom, "turn 2", |dom| control(dom, "Expand README.md").is_some()).await;
    click_control(&mut dom, "Expand all files");
    let text = rendered_text(&dom);
    assert!(text.contains("@@ -1 +1,2 @@") && text.contains("@@ -0,0 +1 @@") && !text.contains("again") && !text.contains("@@ -1 +1,3 @@"), "{text}");
    assert_eq!(control(&dom, "Diff scope").unwrap().1.unwrap(), format!("turn:{}", run_id(state, 2)));

    // The first turn is the whole-thread RPC with a range of one; it changed nothing.
    change_control(&mut dom, "Diff scope", &format!("turn:{}", run_id(state, 1)));
    wait_for_rendered_text(&mut dom, "No net changes in this selection.").await;

    // The whole thread: both files, with turn 3's line included.
    change_control(&mut dom, "Diff scope", "thread");
    until(&mut dom, "whole thread", |dom| control(dom, "Expand all files").is_some()).await;
    click_control(&mut dom, "Expand all files");
    let text = rendered_text(&dom);
    assert!(text.contains("@@ -1 +1,2 @@") && text.contains("@@ -0,0 +1,3 @@") && text.contains("again"), "{text}");
    assert_eq!(error(state), None);

    // Back to the latest turn.
    change_control(&mut dom, "Diff scope", "latest");
    until(&mut dom, "latest again", |dom| control(dom, "Expand hello.txt").is_some() && control(dom, "Expand README.md").is_none()).await;
    fixture.finish(dom).await;
}

pub(crate) fn run_id(state: Store<UiModel>, ordinal: u64) -> String {
    let projection = state.peek().thread.projection.clone().unwrap();
    projection["checkpoints"].as_array().unwrap().iter().find(|checkpoint| checkpoint["appRunOrdinal"] == ordinal).unwrap()["runId"].as_str().unwrap().to_owned()
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn the_whitespace_toggle_refetches_and_reopening_shows_current_data() {
    let fixture = Fixture::start(true).await;
    let gate = Gate::default();
    let (mut dom, props) = fixture.operator(gate.clone());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    three_turns(&fixture, &mut dom, &props, state).await;

    // Close and reopen: nothing is kept, so the panel asks again and shows the newest turn.
    open_latest(&mut dom);
    until(&mut dom, "open", |dom| control(dom, "Expand hello.txt").is_some()).await;
    click_control(&mut dom, "Close diff");
    assert!(control(&dom, "Diff scope").is_none(), "closing removes the panel");
    // Turn 4 adds trailing whitespace to a line and nothing else.
    fixture.write("README.md", "hello\nmore \n");
    turn(&mut dom, &props, state, 4).await;
    gate.hold.set(true);
    // The panel comes back on the selection it had (turn 3, as the original remembers it); the
    // reader moves it to the newest turn, which is turn 4.
    open_latest(&mut dom);
    assert!(control(&dom, "Diff scope").is_some());
    // The reopened panel is waiting on the server rather than showing the turn it showed before.
    assert!(rendered_text(&dom).contains("Loading checkpoint diff..."));
    assert!(control(&dom, "Expand hello.txt").is_none());
    until(&mut dom, "reopen request", |_| gate.held.borrow().iter().any(|(_, release)| !release.is_canceled())).await;
    let live = gate.held.borrow().iter().position(|(_, release)| !release.is_canceled()).unwrap();
    let request = gate.held.borrow()[live].0.clone();
    assert_eq!((request["fromTurnCount"].clone(), request["toTurnCount"].clone(), request["ignoreWhitespace"].clone()), (json!(3), json!(4), json!(true)));
    gate.hold.set(false);
    let (_, release) = gate.held.borrow_mut().remove(live);
    release.send(()).unwrap();
    // Whitespace is ignored by default, so the turn's only change vanishes.
    wait_for_rendered_text(&mut dom, "No net changes in this selection.").await;
    assert!(control(&dom, "Show whitespace changes").is_some());

    // Showing whitespace asks the server again, and the line appears.
    click_control(&mut dom, "Show whitespace changes");
    until(&mut dom, "whitespace shown", |dom| control(dom, "Expand README.md").is_some()).await;
    assert!(control(&dom, "Hide whitespace changes").is_some());
    click_control(&mut dom, "Expand README.md");
    assert!(rendered_text(&dom).contains("@@ -1,2 +1,2 @@"));
    assert!(!rendered_text(&dom).contains("No net changes"));

    // And hiding it again goes back.
    click_control(&mut dom, "Hide whitespace changes");
    wait_for_rendered_text(&mut dom, "No net changes in this selection.").await;
    assert_eq!(error(state), None);
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_project_without_git_says_so_instead_of_failing() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    turn(&mut dom, &props, state, 1).await;
    assert!(!rendered_text(&dom).contains("changed file"));
    open_latest(&mut dom);
    wait_for_rendered_text(&mut dom, "Turn diffs are unavailable because this project is not a git repository.").await;
    assert!(control(&dom, "Retry loading diff").is_none());
    assert_eq!(error(state), None, "an unavailable diff is a state, not an error");
    // Nothing was asked of the server.
    assert!(state.peek().thread.projection.as_ref().unwrap()["checkpoints"].as_array().unwrap().iter().all(|c| c["status"] == "missing"));
    click_control(&mut dom, "Close diff");
    assert!(control(&dom, "Diff scope").is_none());
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_failed_request_shows_its_reason_beside_a_retry_that_works() {
    let fixture = Fixture::start(true).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    three_turns(&fixture, &mut dom, &props, state).await;
    // Take the checkpoint refs away, as an aggressive `git gc` or a manual cleanup would.
    let listing = git(&fixture.workspace, &["for-each-ref", "--format=%(refname) %(objectname)", "refs/t3/orchestration-v2/checkpoints"]);
    let refs: Vec<(String, String)> = listing.lines().map(|line| line.split_once(' ').map(|(name, sha)| (name.to_owned(), sha.to_owned())).unwrap()).collect();
    assert!(refs.len() >= 3, "{refs:?}");
    for (name, _) in &refs {
        git(&fixture.workspace, &["update-ref", "-d", name]);
    }
    open_latest(&mut dom);
    until(&mut dom, "failure", |dom| control(dom, "Retry loading diff").is_some()).await;
    let text = rendered_text(&dom);
    assert!(text.contains("refs/t3/orchestration-v2/checkpoints"), "{}", &text[text.len().saturating_sub(900)..]);
    assert_eq!(error(state), None, "the reason lives in the panel, not the app banner");
    assert!(control(&dom, "Expand all files").is_none());
    // Retrying while it is still broken fails again, visibly.
    click_control(&mut dom, "Retry loading diff");
    until(&mut dom, "second failure", |dom| control(dom, "Retry loading diff").is_some()).await;

    for (name, sha) in &refs {
        git(&fixture.workspace, &["update-ref", name, sha]);
    }
    click_control(&mut dom, "Retry loading diff");
    until(&mut dom, "recovered", |dom| control(dom, "Expand hello.txt").is_some()).await;
    assert!(control(&dom, "Retry loading diff").is_none());
    assert_eq!(error(state), None);
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn moving_the_selection_or_the_thread_mid_load_never_shows_the_old_diff() {
    let fixture = Fixture::start(true).await;
    let gate = Gate::default();
    let (mut dom, props) = fixture.operator(gate.clone());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    three_turns(&fixture, &mut dom, &props, state).await;

    // Turn 3 is requested and held at the server's door.
    gate.hold.set(true);
    open_latest(&mut dom);
    until(&mut dom, "first request", |_| gate.held.borrow().len() == 1).await;
    assert!(rendered_text(&dom).contains("Loading checkpoint diff..."));
    // The reader moves to the whole thread, and then toggles whitespace, before any answer.
    change_control(&mut dom, "Diff scope", "thread");
    until(&mut dom, "second request", |_| gate.held.borrow().len() == 2).await;
    assert!(gate.held.borrow()[0].1.is_canceled(), "the superseded request is cancelled, not left running");
    click_control(&mut dom, "Show whitespace changes");
    until(&mut dom, "third request", |_| gate.held.borrow().len() == 3).await;
    assert!(gate.held.borrow()[1].1.is_canceled());
    assert_eq!(gate.held.borrow()[2].0["ignoreWhitespace"], false);
    assert_eq!(gate.held.borrow()[2].0["toTurnCount"], 3);
    gate.hold.set(false);
    let (_, release) = gate.held.borrow_mut().remove(2);
    release.send(()).unwrap();
    until(&mut dom, "answer", |dom| control(dom, "Expand README.md").is_some()).await;
    // Only the whole-thread answer is on screen: README.md belongs to turn 2, not turn 3.
    click_control(&mut dom, "Expand all files");
    assert!(rendered_text(&dom).contains("@@ -1 +1,2 @@"));

    // A request in flight for thread-a is cancelled when thread-b is opened, and thread-b's
    // panel speaks only for thread-b.
    gate.hold.set(true);
    gate.held.borrow_mut().clear();
    click_control(&mut dom, "Hide whitespace changes");
    until(&mut dom, "held again", |_| gate.held.borrow().len() == 1).await;
    dom.in_scope(ScopeId::APP, || select_thread(&props.transport, state, "thread-b".into()));
    until(&mut dom, "thread-b", |_| state.peek().thread.synchronized && state.peek().thread.projection.as_ref().unwrap()["thread"]["id"] == "thread-b").await;
    // thread-b has no remembered selection, so its panel starts on "Changes".
    change_control(&mut dom, "Diff scope", "latest");
    wait_for_rendered_text(&mut dom, "No completed turns yet.").await;
    assert!(gate.held.borrow()[0].1.is_canceled(), "thread-a's request ended with its panel");
    assert!(!rendered_text(&dom).contains("README.md"));
    assert!(control(&dom, "Expand README.md").is_none());
    assert_eq!(error(state), None);
    fixture.finish(dom).await;
}

/// Counts dynamic attributes called `name` across everything mounted whose text satisfies `keep`.
fn count_attributes(dom: &VirtualDom, name: &str, keep: impl Fn(&str) -> bool) -> usize {
    use dioxus::dioxus_core::{AttributeValue, DynamicNode, VNode};
    fn walk(dom: &VirtualDom, node: &VNode, name: &str, keep: &dyn Fn(&str) -> bool) -> usize {
        let mut count = node
            .dynamic_attrs
            .iter()
            .flat_map(|attributes| attributes.iter())
            .filter(|attribute| attribute.name == name && matches!(&attribute.value, AttributeValue::Text(text) if keep(text)))
            .count();
        for (index, child) in node.dynamic_nodes.iter().enumerate() {
            match child {
                DynamicNode::Component(component) => {
                    if let Some(scope) = component.mounted_scope(index, node, dom) {
                        count += walk(dom, scope.root_node(), name, keep);
                    }
                }
                DynamicNode::Fragment(nodes) => {
                    for node in nodes {
                        count += walk(dom, node, name, keep);
                    }
                }
                _ => {}
            }
        }
        count
    }
    walk(dom, dom.base_scope().root_node(), name, &keep)
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_huge_file_is_drawn_a_window_at_a_time_and_only_the_window_is_coloured() {
    let fixture = Fixture::start(true).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    turn(&mut dom, &props, state, 1).await;
    let source: String = (0..5000).map(|index| format!("fn function_{index}() -> u32 {{ {index} }}\n")).collect();
    fixture.write("big.rs", &source);
    turn(&mut dom, &props, state, 2).await;
    wait_for_rendered_text(&mut dom, "1 changed file").await;
    click_control(&mut dom, "Show diff for big.rs");
    until(&mut dom, "expanded", |dom| control(dom, "Show more lines in big.rs").is_some()).await;

    let rows = |dom: &VirtualDom| count_attributes(dom, "class", |class| class == "diff-row add");
    // The first window: 300 rows including the hunk header, and none of the rest in the page.
    assert_eq!(rows(&dom), 299);
    let text = rendered_text(&dom);
    assert!(text.contains("function_10()") && !text.contains("function_4999()"));
    assert!(text.contains("Show 500 more lines (4701 not shown)"), "{text}");
    // Colour arrives once the grammar has loaded, and covers the window.
    until(&mut dom, "highlight", |dom| count_attributes(dom, "style", |style| style.contains("--syntax-light-fg:")) > 100).await;

    click_control(&mut dom, "Show more lines in big.rs");
    assert_eq!(rows(&dom), 799);
    // Everything, on request: the last line is there, and drawing it did not need the page to
    // have held it before.
    click_control(&mut dom, "Show all lines in big.rs");
    assert_eq!(rows(&dom), 5000);
    assert!(rendered_text(&dom).contains("function_4999()"));
    assert!(control(&dom, "Show more lines in big.rs").is_none());
    // Collapsing releases the rows entirely.
    click_control(&mut dom, "Collapse big.rs");
    assert_eq!(rows(&dom), 0);
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn without_the_read_grant_the_diff_is_not_offered() {
    let fixture = Fixture::start(true).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    three_turns(&fixture, &mut dom, &props, state).await;
    wait_for_rendered_text(&mut dom, "1 changed file").await;
    assert!(control(&dom, "Open diff").is_some() && control(&dom, "Show diff for hello.txt").is_some());
    assert_eq!(control_attribute(&dom, "Diff", "disabled").unwrap().1.as_deref(), Some("false"));
    open_latest(&mut dom);
    until(&mut dom, "diff", |dom| control(dom, "Expand hello.txt").is_some()).await;

    // The destination stops granting orchestration:read: the open panel says so and asks nothing.
    let destination = state.peek().destination.clone().unwrap();
    state.environments().write().revoke(&destination);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    wait_for_rendered_text(&mut dom, "This connection cannot read thread diffs.").await;
    assert!(control(&dom, "Expand hello.txt").is_none());
    click_control(&mut dom, "Close diff");
    assert_eq!(control_attribute(&dom, "Diff", "disabled").unwrap().1.as_deref(), Some("true"));
    // The conversation still lists what changed, but offers no way into a diff it cannot read.
    assert!(control(&dom, "Open diff").is_none() && control(&dom, "Show diff for hello.txt").is_none());
    assert!(control(&dom, "Changed file hello.txt").is_some());
    assert_eq!(error(state), None);
    fixture.finish(dom).await;
}
