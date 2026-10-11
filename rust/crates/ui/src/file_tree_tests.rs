//! The file panel's tree and its other previews through the mounted UI and the real native server over a
//! real workspace: folders are read as they are opened, a file opens from the tree, the search narrows it,
//! a delimited file reads as a table (and as source when asked), a file over a megabyte is cut with the
//! original's notice and never offered for editing, and audio, video and PDF are loaded through signed URLs.
use super::diff_tests::{Fixture, open_thread, rendered_text, seed, until, wait_for_rendered_text};
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::transport_tests::{click_control, click_when_enabled, control, control_attribute, input_control};

const WRITE: [AuthEnvironmentScope; 4] = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::FilesystemRead, AuthEnvironmentScope::FilesystemWrite];

async fn connected(fixture: &Fixture) -> (VirtualDom, Store<UiModel>) {
    let root = &fixture.workspace;
    std::fs::create_dir_all(root.join("src/ui/widgets")).unwrap();
    std::fs::create_dir_all(root.join("docs")).unwrap();
    std::fs::write(root.join("README.md"), "# Readme\n").unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn lib() {}\n").unwrap();
    std::fs::write(root.join("src/ui/widgets/button.rs"), "pub fn button() {}\n").unwrap();
    std::fs::write(root.join("docs/data.csv"), "name,note\n\"Doe, Jane\",\"said \"\"hi\"\"\"\nBob,ok\n").unwrap();
    std::fs::write(root.join("docs/clip.mp4"), b"not really a video").unwrap();
    std::fs::write(root.join("docs/tune.mp3"), b"not really audio").unwrap();
    std::fs::write(root.join("docs/paper.pdf"), b"%PDF-1.4\n").unwrap();
    std::fs::write(root.join("big.txt"), format!("{}\n", "0123456789abcdef".repeat(80_000))).unwrap();
    {
        let (mut dom, props) = fixture.client(Gate::default(), &WRITE);
        seed(&mut dom, &props, &fixture.workspace).await;
    }
    let (mut dom, props) = fixture.client(Gate::default(), &WRITE);
    let state = props.state.borrow().unwrap();
    until(&mut dom, "shell", |_| state.peek().shell.snapshot.as_ref().is_some_and(|snapshot| snapshot.threads.len() == 2) && state.peek().typed_config.is_some()).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    (dom, state)
}
fn open(state: Store<UiModel>, fixture: &Fixture, path: &str) {
    crate::file_panel::open(state, Some("thread-a".to_owned()), fixture.workspace.to_string_lossy().into_owned(), path.into(), None);
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn the_tree_reads_folders_as_they_open_opens_files_and_narrows_by_search() {
    let fixture = Fixture::start(false).await;
    let (mut dom, state) = connected(&fixture).await;
    open(state, &fixture, "README.md");
    wait_for_rendered_text(&mut dom, "Readme").await;
    click_when_enabled(&mut dom, "Show files").await;
    until(&mut dom, "the root", |dom| control(dom, "src").is_some() && control(dom, "README.md").is_some()).await;
    // A folder not opened has not been read.
    assert!(control(&dom, "src/lib.rs").is_none());
    click_when_enabled(&mut dom, "src").await;
    until(&mut dom, "src read", |dom| control(dom, "src/lib.rs").is_some() && control(dom, "src/ui").is_some()).await;
    // A folder that turns out to hold one folder stays open, drawn as one row, and its contents are read.
    click_when_enabled(&mut dom, "src/ui").await;
    until(&mut dom, "ui read", |dom| control(dom, "src/ui/widgets/button.rs").is_some()).await;
    // Choosing a file in the tree opens it in the panel.
    click_when_enabled(&mut dom, "src/lib.rs").await;
    wait_for_rendered_text(&mut dom, "pub fn lib()").await;
    // Collapsing hides what is inside.
    click_when_enabled(&mut dom, "src").await;
    until(&mut dom, "src closed", |dom| control(dom, "src/lib.rs").is_none()).await;
    // The search shows matches under their folders and nothing else.
    let project = fixture.workspace.file_name().unwrap().to_string_lossy().into_owned();
    input_control(&mut dom, &format!("Search {project} files"), "button");
    until(&mut dom, "the match", |dom| control(dom, "src/ui/widgets/button.rs").is_some() && control(dom, "docs").is_none() && control(dom, "src/lib.rs").is_none()).await;
    click_when_enabled(&mut dom, "src/ui/widgets/button.rs").await;
    wait_for_rendered_text(&mut dom, "pub fn button()").await;
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_delimited_file_reads_as_a_table_and_as_source_when_asked() {
    let fixture = Fixture::start(false).await;
    let (mut dom, state) = connected(&fixture).await;
    open(state, &fixture, "docs/data.csv");
    until(&mut dom, "the table", |dom| control(dom, "docs/data.csv").is_some()).await;
    let text = rendered_text(&dom);
    assert!(text.contains("Doe, Jane") && text.contains("said \"hi\""), "quoted cells are one cell each: {text}");
    click_when_enabled(&mut dom, "Show table source").await;
    until(&mut dom, "the source", |dom| control(dom, "docs/data.csv").is_none() && rendered_text(dom).contains("\"Doe, Jane\"")).await;
    click_when_enabled(&mut dom, "Show table").await;
    until(&mut dom, "the table again", |dom| control(dom, "docs/data.csv").is_some()).await;
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_file_over_a_megabyte_is_cut_with_the_notice_and_is_not_offered_for_editing() {
    let fixture = Fixture::start(false).await;
    let (mut dom, state) = connected(&fixture).await;
    open(state, &fixture, "big.txt");
    let size = std::fs::metadata(fixture.workspace.join("big.txt")).unwrap().len();
    let notice = t3_client::file_preview::limit_notice(size);
    wait_for_rendered_text(&mut dom, &notice).await;
    assert!(control(&dom, "Edit big.txt").is_none(), "the original never edits the start of a larger file");
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn audio_video_and_pdf_load_through_signed_urls_of_the_server() {
    let fixture = Fixture::start(false).await;
    let (mut dom, state) = connected(&fixture).await;
    for (path, label, fragment) in [("docs/tune.mp3", "Audio tune.mp3", false), ("docs/clip.mp4", "Video clip.mp4", false), ("docs/paper.pdf", "Document paper.pdf", true)] {
        open(state, &fixture, path);
        until(&mut dom, label, |dom| control(dom, label).is_some()).await;
        let (_, source) = control_attribute(&dom, label, "src").unwrap();
        let source = source.unwrap();
        assert!(source.starts_with("http://127.0.0.1:") && source.contains("/api/"), "a signed URL of this server: {source}");
        assert_eq!(source.ends_with("#toolbar=0&view=FitH"), fragment, "only the PDF viewer is asked for the page alone");
        click_control(&mut dom, "Close file");
        until(&mut dom, "closed", |dom| control(dom, "Close file").is_none()).await;
    }
    fixture.finish(dom).await;
}
