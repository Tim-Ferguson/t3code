//! Editing a file in the panel through the mounted UI and the real native server over a real workspace: the edit is written a
//! moment after typing stops, the tab marks the file until it is on disk, closing the panel writes what was left, a connection
//! without the write grant gets no editor, and a file the editor would be too slow for stays a view.
use super::diff_tests::{Fixture, open_thread, rendered_text, seed, until, wait_for_rendered_text};
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::transport_tests::{click_when_enabled, control, control_attribute, input_control};

const READ: [AuthEnvironmentScope; 3] = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::FilesystemRead];
const WRITE: [AuthEnvironmentScope; 4] = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::FilesystemRead, AuthEnvironmentScope::FilesystemWrite];

async fn connected(fixture: &Fixture, scopes: &[AuthEnvironmentScope]) -> (VirtualDom, Store<UiModel>) {
    std::fs::create_dir_all(fixture.workspace.join("src")).unwrap();
    std::fs::write(fixture.workspace.join("src/notes.txt"), "first line\nsecond line\n").unwrap();
    std::fs::write(fixture.workspace.join("src/other.txt"), "other file\n").unwrap();
    // The thread is made by a client that may operate; the one under test may have fewer grants.
    {
        let (mut dom, props) = fixture.client(Gate::default(), &WRITE);
        seed(&mut dom, &props, &fixture.workspace).await;
    }
    let (mut dom, props) = fixture.client(Gate::default(), scopes);
    let state = props.state.borrow().unwrap();
    until(&mut dom, "shell", |_| state.peek().shell.snapshot.as_ref().is_some_and(|snapshot| snapshot.threads.len() == 2) && state.peek().typed_config.is_some()).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    (dom, state)
}

fn open(state: Store<UiModel>, fixture: &Fixture, path: &str) {
    crate::file_panel::open(state, Some("thread-a".to_owned()), fixture.workspace.to_string_lossy().into_owned(), path.into(), None);
}

#[tokio::test(flavor = "current_thread")]
async fn typing_in_the_editor_saves_a_moment_later_and_the_tab_marks_the_file_until_it_has() {
    let fixture = Fixture::start(false).await;
    let (mut dom, state) = connected(&fixture, &WRITE).await;
    open(state, &fixture, "src/notes.txt");
    wait_for_rendered_text(&mut dom, "second line").await;
    // A second file gives the row of tabs; edit it.
    open(state, &fixture, "src/other.txt");
    wait_for_rendered_text(&mut dom, "other file").await;
    click_when_enabled(&mut dom, "Edit other.txt").await;
    until(&mut dom, "the editor", |dom| control(dom, "Stop editing other.txt").is_some()).await;
    input_control(&mut dom, "Edit other.txt", "first draft");
    input_control(&mut dom, "Edit other.txt", "other file, changed\n");
    assert!(!state.peek().file_pending.is_empty());
    until(&mut dom, "marked as unsaved", |dom| control(dom, "Unsaved changes").is_some() || state.peek().file_pending.is_empty()).await;
    // The milestone is the marker leaving: the write reached the disk before that.
    until(&mut dom, "saved", |dom| control(dom, "Unsaved changes").is_none() && state.peek().file_pending.is_empty()).await;
    assert_eq!(std::fs::read_to_string(fixture.workspace.join("src/other.txt")).unwrap(), "other file, changed\n", "only the latest contents were written");
    assert_eq!(std::fs::read_to_string(fixture.workspace.join("src/notes.txt")).unwrap(), "first line\nsecond line\n", "the other file is untouched");
    assert!(state.peek().file_unsaved.is_empty());
    // Back to reading: the view shows what is on disk.
    click_when_enabled(&mut dom, "Stop editing other.txt").await;
    wait_for_rendered_text(&mut dom, "other file, changed").await;
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn closing_the_panel_writes_an_edit_that_was_still_waiting_and_reopening_shows_it() {
    let fixture = Fixture::start(false).await;
    let (mut dom, state) = connected(&fixture, &WRITE).await;
    open(state, &fixture, "src/notes.txt");
    wait_for_rendered_text(&mut dom, "second line").await;
    click_when_enabled(&mut dom, "Edit notes.txt").await;
    until(&mut dom, "the editor", |dom| control(dom, "Stop editing notes.txt").is_some()).await;
    input_control(&mut dom, "Edit notes.txt", "written on close\n");
    // Closed inside the debounce window: the panel's end writes it.
    click_when_enabled(&mut dom, "Close file").await;
    until(&mut dom, "the panel gone", |dom| control(dom, "Close file").is_none()).await;
    // Opened again at once, before the write need have finished: it shows the edit, not the file as it was, and the header says
    // it is still being saved until it is.
    open(state, &fixture, "src/notes.txt");
    wait_for_rendered_text(&mut dom, "written on close").await;
    until(&mut dom, "saved", |dom| !rendered_text(dom).contains("Saving\u{2026}") && state.peek().file_pending.is_empty()).await;
    assert_eq!(std::fs::read_to_string(fixture.workspace.join("src/notes.txt")).unwrap(), "written on close\n");
    assert!(state.peek().file_unsaved.is_empty());
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn without_the_write_grant_or_for_a_big_file_there_is_no_editor() {
    let fixture = Fixture::start(false).await;
    let (mut dom, state) = connected(&fixture, &READ).await;
    open(state, &fixture, "src/notes.txt");
    wait_for_rendered_text(&mut dom, "second line").await;
    assert!(control(&dom, "Edit notes.txt").is_none(), "reading is not editing");
    fixture.finish(dom).await;

    let fixture = Fixture::start(false).await;
    let (mut dom, state) = connected(&fixture, &WRITE).await;
    // Past the limit in short lines, as a real file is.
    let line = format!("{}\n", "x".repeat(49));
    std::fs::write(fixture.workspace.join("src/big.txt"), line.repeat(t3_client::project_files::EDIT_LIMIT_BYTES as usize / 50 + 1)).unwrap();
    open(state, &fixture, "src/big.txt");
    until(&mut dom, "the file drawn", |dom| control(dom, "Contents of big.txt").is_some()).await;
    assert!(control(&dom, "Edit big.txt").is_none());
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_comment_on_a_line_of_the_file_becomes_a_chip_on_the_threads_message_and_needs_words_first() {
    let fixture = Fixture::start(false).await;
    let (mut dom, state) = connected(&fixture, &READ).await;
    open(state, &fixture, "src/notes.txt");
    wait_for_rendered_text(&mut dom, "second line").await;
    click_when_enabled(&mut dom, "Comment on line 2").await;
    until(&mut dom, "the comment box", |dom| control(dom, "File comment").is_some()).await;
    // Nothing is added until something is written.
    assert_eq!(control_attribute(&dom, "Add comment to chat", "disabled").and_then(|(_, value)| value).as_deref(), Some("true"));
    input_control(&mut dom, "File comment", "  why second?  ");
    click_when_enabled(&mut dom, "Add comment to chat").await;
    until(&mut dom, "the box closed", |dom| control(dom, "File comment").is_none()).await;
    wait_for_rendered_text(&mut dom, "notes.txt L2").await;
    let environment = state.peek().destination.as_ref().unwrap().to_string();
    let chips = state.peek().draft_contexts.get(&t3_client::draft_storage::DraftTarget::thread(environment, "thread-a")).cloned().unwrap_or_default();
    assert_eq!(chips.len(), 1);
    let chip = &chips[0];
    assert_eq!((chip["kind"].as_str(), chip["filePath"].as_str(), chip["rangeLabel"].as_str(), chip["diff"].as_str(), chip["text"].as_str()), (Some("review-comment"), Some("src/notes.txt"), Some("L2"), Some("second line"), Some("why second?")));
    fixture.finish(dom).await;
}
