//! The tabbed panel through the mounted UI and the real native server: the shortcuts that show, hide, grow and close it, the
//! launcher and the add menu, a panel for each thread that comes back after a reload, and the tab that "Reopen closed tab"
//! brings back in its own thread. Keys reach the app the way the page's listener sends them; waits are on text or state only
//! the awaited step produces.
use super::diff_tests::{Fixture, open_thread, rendered_text, seed, until};
use super::*;
use crate::diff_panel::Gate;
use crate::right_panel::MEMORY;
use crate::runtime::keys_tests::{chord, key_on, press};
use crate::runtime::transport_tests::{click_control, click_when_enabled, control, control_attribute, mouse_control};
use t3_client::keybindings::Dom;
use t3_client::right_panel::{Kind, Surface};

const OPEN: &str = "Open a surface";

fn tabs(state: Store<UiModel>, thread: &str) -> Vec<String> {
    let environment = state.peek().destination.as_ref().unwrap().to_string();
    let key = format!("{environment}:{thread}");
    state.peek().right_panel.panels.get(&key).surfaces.iter().map(Surface::id).collect()
}
fn shown(state: Store<UiModel>, thread: &str) -> Option<String> {
    let environment = state.peek().destination.as_ref().unwrap().to_string();
    let key = format!("{environment}:{thread}");
    state.peek().right_panel.panels.active(&key).map(Surface::id)
}
fn hidden_but_kept(state: Store<UiModel>, thread: &str) -> bool {
    let environment = state.peek().destination.as_ref().unwrap().to_string();
    let panel = state.peek().right_panel.panels.get(&format!("{environment}:{thread}")).clone();
    !panel.is_open && !panel.surfaces.is_empty()
}
async fn ready(fixture: &Fixture) -> (VirtualDom, super::diff_tests::Harness, Store<UiModel>) {
    MEMORY.with(|memory| memory.borrow_mut().clear());
    std::fs::write(fixture.workspace.join("README.md"), "# Readme\n").unwrap();
    let (mut dom, props) = fixture.client(Gate::default(), &[AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::FilesystemRead]);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    dom.in_scope(ScopeId::APP, || spawn(crate::right_panel::hydrate(state)));
    // Reading storage draws nothing, so there is no change to wait on: let the task run.
    crate::runtime::diff_tests::pump(&mut dom).await;
    assert!(state.peek().right_panel.hydrated, "storage was read");
    (dom, props, state)
}
/// Replaces the environment's keybindings file the way an editor save does.
fn write_keybindings(fixture: &Fixture, contents: &str) {
    let path = fixture.directory.path().join("keybindings.json");
    let staging = path.with_extension("tmp");
    std::fs::write(&staging, contents).unwrap();
    std::fs::rename(staging, path).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn the_shortcut_shows_an_empty_panel_with_its_launcher_and_a_row_a_letter_or_the_add_menu_opens_a_tab() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props, state) = ready(&fixture).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    assert!(control(&dom, OPEN).is_none() && control(&dom, "Toggle panel").is_some());

    // The shortcut shows a panel with nothing in it: the launcher lists what can be opened, and says why the rest cannot.
    assert_eq!(press(&mut dom, chord("b", false, true), Dom::default()).await.as_deref(), Some("rightPanel.toggle"));
    until(&mut dom, "the launcher", |dom| control(dom, OPEN).is_some()).await;
    assert!(control(&dom, "Open Files").is_some() && control(&dom, "Open Diff").is_some());
    assert!(control(&dom, "Open Pull request").is_none() && rendered_text(&dom).contains("No pull request on this branch yet."));
    assert_eq!(control_attribute(&dom, "Toggle panel", "aria-pressed").and_then(|(_, value)| value).as_deref(), Some("true"));
    assert!(tabs(state, "thread-a").is_empty());

    // A letter opens its row: Files, which is the project's tree on its own.
    key_on(&mut dom, OPEN, Key::Character("f".into()));
    until(&mut dom, "the Files tab", |dom| control(dom, "Close Files tab").is_some() && control(dom, OPEN).is_none()).await;
    until(&mut dom, "the tree", |dom| control(dom, "README.md").is_some()).await;
    assert_eq!(shown(state, "thread-a").as_deref(), Some("files"));

    // `rightPanel.new` opens the add menu; a row of it opens its tab beside the others, and the menu closes.
    assert_eq!(press(&mut dom, chord("t", false, false), Dom::default()).await.as_deref(), Some("rightPanel.new"));
    until(&mut dom, "the menu", |dom| control(dom, "Add Diff").is_some()).await;
    assert_eq!(control_attribute(&dom, "Add Pull request", "disabled").and_then(|(_, value)| value).as_deref(), Some("true"), "a row that cannot be opened is in the menu, switched off");
    click_control(&mut dom, "Add Diff");
    until(&mut dom, "the Diff tab", |dom| control(dom, "Close Diff tab").is_some() && control(dom, "Add Diff").is_none()).await;
    assert_eq!(tabs(state, "thread-a"), ["files", "diff"]);
    assert_eq!(shown(state, "thread-a").as_deref(), Some("diff"));
    until(&mut dom, "the diff", |dom| control(dom, "Diff panel").is_some()).await;

    // A key in the menu opens its row too; Escape ends the menu and opens nothing.
    assert_eq!(press(&mut dom, chord("t", false, false), Dom::default()).await.as_deref(), Some("rightPanel.new"));
    until(&mut dom, "the menu again", |dom| control(dom, "Add a tab").is_some()).await;
    key_on(&mut dom, "Add a tab", Key::Escape);
    until(&mut dom, "no menu", |dom| control(dom, "Add a tab").is_none()).await;
    assert_eq!(tabs(state, "thread-a"), ["files", "diff"]);

    // A tab is chosen by clicking it, which shows its body.
    click_control(&mut dom, "Files");
    until(&mut dom, "Files again", |dom| control(dom, "Diff panel").is_none() && control(dom, "README.md").is_some()).await;
    assert_eq!(shown(state, "thread-a").as_deref(), Some("files"));
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_file_replaces_the_files_tab_and_the_panel_grows_hides_and_closes_its_tabs() {
    let fixture = Fixture::start(false).await;
    write_keybindings(&fixture, r#"[{"key":"mod+shift+y","command":"rightPanel.toggleMaximized"}]"#);
    let (mut dom, props, state) = ready(&fixture).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    until(&mut dom, "the keybindings", |_| state.peek().typed_config.as_ref().is_some_and(|config| config.keybindings.0.iter().any(|rule| rule.command.as_str() == "rightPanel.toggleMaximized"))).await;

    // Nothing is open, so the keys that act on an open panel are the browser's own.
    assert_eq!(press(&mut dom, chord("w", false, false), Dom::default()).await, None);
    assert_eq!(press(&mut dom, chord("t", false, false), Dom::default()).await, None);
    assert_eq!(press(&mut dom, chord("y", true, false), Dom::default()).await, None);

    dom.in_scope(ScopeId::APP, || crate::right_panel::open_kind(state, Kind::Files));
    until(&mut dom, "the tree", |dom| control(dom, "README.md").is_some()).await;
    // A file chosen in the tree is a tab, and the standalone tree gives way to it.
    click_control(&mut dom, "README.md");
    until(&mut dom, "the file tab", |dom| control(dom, "Close README.md tab").is_some() && control(dom, "Close Files tab").is_none()).await;
    assert_eq!(tabs(state, "thread-a"), ["file:README.md"]);
    // The diff beside it, by the header's button.
    click_when_enabled(&mut dom, "Diff").await;
    until(&mut dom, "the diff tab", |dom| control(dom, "Close Diff tab").is_some()).await;
    assert_eq!(tabs(state, "thread-a"), ["file:README.md", "diff"]);

    // Maximizing is a state of this thread's panel: a shortcut and the button toggle it.
    assert_eq!(control_attribute(&dom, "Panel", "data-maximized").and_then(|(_, value)| value).as_deref(), Some("false"));
    assert_eq!(press(&mut dom, chord("y", true, false), Dom::default()).await.as_deref(), Some("rightPanel.toggleMaximized"));
    until(&mut dom, "maximized", |dom| control_attribute(dom, "Panel", "data-maximized").and_then(|(_, value)| value).as_deref() == Some("true")).await;
    click_control(&mut dom, "Restore panel");
    until(&mut dom, "restored", |dom| control_attribute(dom, "Panel", "data-maximized").and_then(|(_, value)| value).as_deref() == Some("false")).await;

    // The close shortcut closes the shown tab; its neighbour is shown.
    assert_eq!(press(&mut dom, chord("w", false, false), Dom::default()).await.as_deref(), Some("rightPanel.close"));
    until(&mut dom, "the diff closed", |dom| control(dom, "Close Diff tab").is_none()).await;
    assert_eq!(shown(state, "thread-a").as_deref(), Some("file:README.md"));

    // Hiding keeps the tabs; the same button and the shortcut bring the panel back as it was.
    click_control(&mut dom, "Hide panel");
    until(&mut dom, "hidden", |dom| control(dom, "Close README.md tab").is_none()).await;
    assert!(hidden_but_kept(state, "thread-a"));
    assert_eq!(press(&mut dom, chord("w", false, false), Dom::default()).await, None, "a hidden panel leaves the key alone");
    assert_eq!(press(&mut dom, chord("b", false, true), Dom::default()).await.as_deref(), Some("rightPanel.toggle"));
    until(&mut dom, "shown again", |dom| control(dom, "Close README.md tab").is_some()).await;

    // The menu of a tab closes the others, the ones to its right or all of them.
    dom.in_scope(ScopeId::APP, || crate::right_panel::open_kind(state, Kind::Diff));
    until(&mut dom, "the diff tab again", |dom| control(dom, "Close Diff tab").is_some()).await;
    mouse_control(&mut dom, "README.md", "contextmenu", false, false, false);
    until(&mut dom, "the tab menu", |dom| control(dom, "Tab actions").is_some()).await;
    assert_eq!(control_attribute(&dom, "Close to the right", "disabled").and_then(|(_, value)| value).as_deref(), Some("false"));
    click_when_enabled(&mut dom, "Close to the right").await;
    until(&mut dom, "the tab to its right gone", |dom| control(dom, "Close Diff tab").is_none()).await;
    mouse_control(&mut dom, "README.md", "contextmenu", false, false, false);
    until(&mut dom, "the tab menu again", |dom| control(dom, "Tab actions").is_some()).await;
    assert!(control(&dom, "Copy path").is_some(), "a file's tab can copy its path");
    click_when_enabled(&mut dom, "Close all").await;
    until(&mut dom, "no panel", |dom| control(dom, "Panel").is_none()).await;
    assert!(tabs(state, "thread-a").is_empty() && !crate::right_panel::is_open_peek(state));
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn each_thread_has_its_own_panel_which_a_reload_finds_and_reopen_brings_a_closed_tab_back_in_its_thread() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props, state) = ready(&fixture).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    dom.in_scope(ScopeId::APP, || crate::right_panel::open_kind(state, Kind::Diff));
    until(&mut dom, "the diff in thread-a", |dom| control(dom, "Diff panel").is_some()).await;

    // Another thread starts with none, and gets its own.
    open_thread(&mut dom, &props, state, "thread-b").await;
    until(&mut dom, "no panel in thread-b", |dom| control(dom, "Diff panel").is_none() && control(dom, "Panel").is_none()).await;
    dom.in_scope(ScopeId::APP, || crate::right_panel::open_kind(state, Kind::Files));
    until(&mut dom, "files in thread-b", |dom| control(dom, "Close Files tab").is_some()).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    until(&mut dom, "thread-a's diff", |dom| control(dom, "Close Diff tab").is_some() && control(dom, "Close Files tab").is_none()).await;
    assert_eq!((tabs(state, "thread-a"), tabs(state, "thread-b")), (vec!["diff".to_owned()], vec!["files".to_owned()]));

    // What storage holds is what a reload reads: the same tabs, in the same threads, hidden stays hidden.
    until(&mut dom, "the write", |_| MEMORY.with(|memory| memory.borrow().get(t3_client::right_panel::STORAGE_KEY).is_some_and(|raw| raw.contains("\"diff\"") || raw.contains("\"kind\":\"diff\"")))).await;
    let written = MEMORY.with(|memory| memory.borrow().get(t3_client::right_panel::STORAGE_KEY).cloned()).unwrap();
    state.right_panel().write().panels = Default::default();
    until(&mut dom, "nothing shown", |dom| control(dom, "Diff panel").is_none()).await;
    MEMORY.with(|memory| memory.borrow_mut().insert(t3_client::right_panel::STORAGE_KEY.to_owned(), written));
    dom.in_scope(ScopeId::APP, || spawn(crate::right_panel::hydrate(state)));
    until(&mut dom, "the diff after the reload", |dom| control(dom, "Close Diff tab").is_some()).await;
    assert_eq!((tabs(state, "thread-a"), tabs(state, "thread-b")), (vec!["diff".to_owned()], vec!["files".to_owned()]));

    // Closing the diff remembers it; with another thread open, the reopen shortcut opens thread-a again with its diff back.
    click_control(&mut dom, "Close Diff tab");
    until(&mut dom, "closed", |dom| control(dom, "Diff panel").is_none() && control(dom, "Panel").is_none()).await;
    assert!(crate::right_panel::can_reopen_peek(state));
    open_thread(&mut dom, &props, state, "thread-b").await;
    until(&mut dom, "thread-b's files", |dom| control(dom, "Close Files tab").is_some()).await;
    assert_eq!(press(&mut dom, chord("t", true, false), Dom::default()).await.as_deref(), Some("view.reopenClosed"));
    until(&mut dom, "thread-a with its diff", |dom| state.peek().active_thread.as_deref() == Some("thread-a") && control(dom, "Close Diff tab").is_some()).await;
    assert!(!crate::right_panel::can_reopen_peek(state), "the entry is used up");
    // With nothing left to reopen the shortcut is the browser's.
    assert_eq!(press(&mut dom, chord("t", true, false), Dom::default()).await, None);
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_thread_that_is_deleted_takes_its_panel_with_it() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props, state) = ready(&fixture).await;
    open_thread(&mut dom, &props, state, "thread-b").await;
    dom.in_scope(ScopeId::APP, || crate::right_panel::open_kind(state, Kind::Files));
    open_thread(&mut dom, &props, state, "thread-a").await;
    dom.in_scope(ScopeId::APP, || crate::right_panel::open_kind(state, Kind::Diff));
    until(&mut dom, "the diff", |dom| control(dom, "Diff panel").is_some()).await;
    // Deleting the open thread, from its header, as the person would.
    click_control(&mut dom, "Delete thread");
    click_control(&mut dom, "Confirm");
    until(&mut dom, "the panel dropped", |_| tabs(state, "thread-a").is_empty()).await;
    assert_eq!(tabs(state, "thread-b"), ["files"], "another thread's panel is left alone");
    fixture.finish(dom).await;
}
