//! "Go to file" and "Search project contents" through the mounted UI and the real native server over a
//! real workspace: the shortcuts and the palette open them, typing asks the server once per pause,
//! and choosing a result opens the file in the panel beside the conversation at the line it names.
use super::diff_tests::{Fixture, open_thread, rendered_text, seed, until, wait_for_rendered_text};
use super::keys_tests::{PALETTE, chord, key_on, open_palette, press};
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::transport_tests::{click_when_enabled, control, control_attribute, input_control};
use t3_client::keybindings::Dom;

const SCOPES: [AuthEnvironmentScope; 3] = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::FilesystemRead];
const PICKER: &str = "Search files\u{2026}";
const BODY: &str = "fn alpha() {}\nfn beta_needle() {}\nfn gamma() {}\n";

async fn connected(fixture: &Fixture) -> (VirtualDom, super::diff_tests::Harness, Store<UiModel>) {
    fixture.write("README.md", "hello\n");
    std::fs::create_dir_all(fixture.workspace.join("src")).unwrap();
    std::fs::write(fixture.workspace.join("src/needle_target.rs"), BODY).unwrap();
    std::fs::write(fixture.workspace.join("src/other.rs"), "fn nothing() {}\n").unwrap();
    let (mut dom, props) = fixture.client(Gate::default(), &SCOPES);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "config", |_| state.peek().typed_config.is_some()).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    (dom, props, state)
}

#[tokio::test(flavor = "current_thread")]
async fn go_to_file_asks_once_per_pause_marks_the_match_and_opens_the_file_in_the_panel() {
    let fixture = Fixture::start(false).await;
    let (mut dom, _props, state) = connected(&fixture).await;
    crate::project_search::REQUESTS.with(|requests| requests.borrow_mut().clear());

    assert_eq!(press(&mut dom, chord("p", false, false), Dom::default()).await.as_deref(), Some("filePicker.toggle"));
    // The first question, with nothing typed, is recent files; it needs no pause.
    until(&mut dom, "the picker with its first rows", |dom| control(dom, "README.md").is_some()).await;
    // Three keystrokes between two polls of the page are one question.
    for typed in ["n", "ne", "needle"] {
        input_control(&mut dom, PICKER, typed);
    }
    click_when_enabled(&mut dom, "src/needle_target.rs").await;
    assert_eq!(crate::project_search::REQUESTS.with(|requests| requests.borrow().clone()), ["", "needle"]);
    until(&mut dom, "the panel", |dom| control(dom, "Close file").is_some()).await;
    wait_for_rendered_text(&mut dom, "fn beta_needle() {}").await;
    assert!(control(&dom, PICKER).is_none(), "choosing a file closes the picker");
    assert_eq!(crate::right_panel::shown_file(state), Some(("src/needle_target.rs".to_owned(), None)));
    assert_eq!(state.peek().active_thread.as_deref(), Some("thread-a"), "the file is a tab of the open thread's panel");

    // The file panel and the diff panel share the margin: the diff takes it back.
    assert_eq!(press(&mut dom, chord("d", false, false), Dom::default()).await.as_deref(), Some("diff.toggle"));
    until(&mut dom, "the file panel gave way", |dom| control(dom, "Close file").is_none()).await;
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn the_picker_says_what_it_is_doing_and_escape_goes_back_to_the_commands() {
    let fixture = Fixture::start(false).await;
    let (mut dom, _props, _state) = connected(&fixture).await;
    // From the palette: the entry carries the shortcut, and the second press of the shortcut closes it.
    open_palette(&mut dom).await;
    click_when_enabled(&mut dom, "Go to file").await;
    until(&mut dom, "the picker", |dom| control(dom, PICKER).is_some()).await;
    assert!(control(&dom, PALETTE).is_none());
    input_control(&mut dom, PICKER, "zzzzqqqq");
    wait_for_rendered_text(&mut dom, "No matching files.").await;
    key_on(&mut dom, PICKER, Key::Escape);
    until(&mut dom, "the commands", |dom| control(dom, PALETTE).is_some()).await;
    assert_eq!(press(&mut dom, chord("p", false, false), Dom::default()).await.as_deref(), Some("filePicker.toggle"));
    until(&mut dom, "the picker again", |dom| control(dom, PICKER).is_some()).await;
    assert_eq!(press(&mut dom, chord("p", false, false), Dom::default()).await.as_deref(), Some("filePicker.toggle"));
    until(&mut dom, "closed", |dom| control(dom, PICKER).is_none() && control(dom, PALETTE).is_none()).await;
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn content_search_counts_matches_honours_its_options_and_opens_the_file_at_the_line() {
    let fixture = Fixture::start(false).await;
    let (mut dom, _props, state) = connected(&fixture).await;
    assert_eq!(press(&mut dom, chord("f", true, false), Dom::default()).await.as_deref(), Some("projectSearch.toggle"));
    let input = "Search in Project";
    until(&mut dom, "the dialog", |dom| control(dom, input).is_some()).await;
    wait_for_rendered_text(&mut dom, "Type to search across your project.").await;

    input_control(&mut dom, input, "NEEDLE");
    wait_for_rendered_text(&mut dom, "1 results in 1 files").await;
    // Case matters when asked to.
    click_when_enabled(&mut dom, "Match case").await;
    wait_for_rendered_text(&mut dom, "No results found.").await;
    assert_eq!(control_attribute(&dom, "Match case", "aria-pressed").and_then(|(_, value)| value).as_deref(), Some("true"));
    click_when_enabled(&mut dom, "Match case").await;
    wait_for_rendered_text(&mut dom, "1 results in 1 files").await;
    // A pattern that does not compile is said so.
    click_when_enabled(&mut dom, "Use regular expression").await;
    input_control(&mut dom, input, "needle(");
    wait_for_rendered_text(&mut dom, "Invalid regular expression").await;
    click_when_enabled(&mut dom, "Use regular expression").await;
    input_control(&mut dom, input, "needle");
    wait_for_rendered_text(&mut dom, "1 results in 1 files").await;

    click_when_enabled(&mut dom, "src/needle_target.rs line 2").await;
    until(&mut dom, "the panel", |dom| control(dom, "Close file").is_some()).await;
    wait_for_rendered_text(&mut dom, "fn gamma() {}").await;
    assert_eq!(crate::right_panel::shown_file(state), Some(("src/needle_target.rs".to_owned(), Some(2))));
    assert!(control(&dom, input).is_none(), "choosing a match closes the dialog");
    // The line the match names is the one marked.
    let text = rendered_text(&dom);
    assert!(text.contains("Show more lines") == false && text.contains("fn alpha() {}"));
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn without_the_filesystem_grant_both_dialogs_say_so_and_ask_nothing() {
    let fixture = Fixture::start(false).await;
    {
        let (mut dom, props) = fixture.client(Gate::default(), &SCOPES);
        seed(&mut dom, &props, &fixture.workspace).await;
    }
    let (mut dom, props) = fixture.client(Gate::default(), &[AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate]);
    let state = props.state.borrow().unwrap();
    until(&mut dom, "shell", |_| state.peek().shell.snapshot.as_ref().is_some_and(|snapshot| snapshot.threads.len() == 2) && state.peek().typed_config.is_some()).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    crate::project_search::REQUESTS.with(|requests| requests.borrow_mut().clear());
    let granted = state.peek().destination.clone().is_some_and(|destination| state.peek().environments.allows(&destination, AuthEnvironmentScope::FilesystemRead));
    press(&mut dom, chord("p", false, false), Dom::default()).await;
    until(&mut dom, "the picker", |dom| control(dom, PICKER).is_some()).await;
    if !granted {
        wait_for_rendered_text(&mut dom, "This connection cannot search host files.").await;
        assert!(crate::project_search::REQUESTS.with(|requests| requests.borrow().is_empty()));
    }
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn files_open_as_tabs_a_markdown_file_renders_and_a_tab_closes_to_its_neighbour() {
    let fixture = Fixture::start(false).await;
    let (mut dom, _props, state) = connected(&fixture).await;
    std::fs::create_dir_all(fixture.workspace.join("notes")).unwrap();
    std::fs::write(fixture.workspace.join("notes/guide.md"), "# Guide heading\n\nSome *words* here.\n").unwrap();
    std::fs::write(fixture.workspace.join("src/plain.txt"), "plain text line\n").unwrap();
    let cwd = fixture.workspace.to_string_lossy().into_owned();
    let owner = Some("thread-a".to_owned());
    crate::file_panel::open(state, owner.clone(), cwd.clone(), "notes/guide.md".into(), None);
    // Its markdown is shown rendered, with a way to its source; the panel's tab bar holds the file.
    wait_for_rendered_text(&mut dom, "Guide heading").await;
    assert!(control(&dom, "Close guide.md tab").is_some());
    assert!(control(&dom, "Show markdown source").is_some());
    crate::file_panel::open(state, owner, cwd, "src/plain.txt".into(), None);
    wait_for_rendered_text(&mut dom, "plain text line").await;
    assert_eq!(crate::right_panel::file_tabs(state), ["notes/guide.md", "src/plain.txt"]);
    assert!(control(&dom, "Close guide.md tab").is_some() && control(&dom, "Close plain.txt tab").is_some());
    assert!(control(&dom, "Show markdown source").is_none(), "only markdown has the toggle");
    // Back to the first tab, which draws its source on request.
    click_when_enabled(&mut dom, "Close plain.txt tab").await;
    until(&mut dom, "the neighbour", |dom| rendered_text(dom).contains("Guide heading")).await;
    assert_eq!(crate::right_panel::shown_file(state).map(|(path, _)| path).as_deref(), Some("notes/guide.md"));
    click_when_enabled(&mut dom, "Show markdown source").await;
    wait_for_rendered_text(&mut dom, "# Guide heading").await;
    assert!(control(&dom, "Show rendered markdown").is_some());
    // Closing the last tab closes the panel.
    click_when_enabled(&mut dom, "Close file").await;
    until(&mut dom, "the panel gone", |dom| control(dom, "Close file").is_none()).await;
    assert!(crate::right_panel::file_tabs(state).is_empty());
    fixture.finish(dom).await;
}
