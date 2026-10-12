//! The folder browser of the Add project dialog against the real native server's `filesystem.browse`: typing a path lists the folders
//! under it, narrowed by what has been typed of the next name, hidden folders only for a dot; a click goes one folder down, "Up one
//! level" goes up, and a connection without the filesystem grant is told it cannot browse.
use super::diff_tests::{Fixture, rendered_text, seed, until};
use super::keys_tests::open_palette;
use super::transport_tests::{click_control, control, input_control};
use super::*;
use crate::diff_panel::Gate;

async fn open_dialog(scopes: &[AuthEnvironmentScope]) -> (Fixture, VirtualDom, Store<UiModel>) {
    let fixture = Fixture::start(false).await;
    for folder in ["alpha", "alpine", "beta", ".hidden"] {
        std::fs::create_dir(fixture.workspace.join(folder)).unwrap();
    }
    // A file is not a folder: it is never offered.
    std::fs::write(fixture.workspace.join("alpha.txt"), "x").unwrap();
    let (mut dom, props) = fixture.client(Gate::default(), scopes);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the configuration", |_| state.peek().typed_config.is_some()).await;
    open_palette(&mut dom).await;
    click_control(&mut dom, "Add project");
    until(&mut dom, "the dialog", |dom| control(dom, "Project directory on the server").is_some()).await;
    (fixture, dom, state)
}

#[tokio::test(flavor = "current_thread")]
async fn typing_a_path_lists_its_folders_narrowed_by_the_next_name_and_a_click_goes_into_one() {
    let scopes = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::FilesystemRead];
    let (fixture, mut dom, _state) = open_dialog(&scopes).await;
    let root = format!("{}/", fixture.workspace.display());
    // A name is not a path: nothing is listed for it.
    input_control(&mut dom, "Project directory on the server", "alpha");
    assert!(control(&dom, "Folders on the server").is_none());
    input_control(&mut dom, "Project directory on the server", &root);
    until(&mut dom, "the folders", |dom| control(dom, "Open folder alpha").is_some() && control(dom, "Open folder beta").is_some()).await;
    assert!(control(&dom, "Open folder alpine").is_some() && control(&dom, "Open folder .hidden").is_none() && control(&dom, "Open folder alpha.txt").is_none(), "{}", rendered_text(&dom));
    // What is typed of the next name narrows the list without asking again.
    input_control(&mut dom, "Project directory on the server", &format!("{root}alp"));
    until(&mut dom, "narrowed", |dom| control(dom, "Open folder beta").is_none()).await;
    assert!(control(&dom, "Open folder alpha").is_some() && control(&dom, "Open folder alpine").is_some());
    input_control(&mut dom, "Project directory on the server", &format!("{root}."));
    until(&mut dom, "hidden", |dom| control(dom, "Open folder .hidden").is_some()).await;
    // A click goes into the folder: its name is added to the path and its own folders are listed.
    std::fs::create_dir(fixture.workspace.join("alpha").join("inner")).unwrap();
    input_control(&mut dom, "Project directory on the server", &format!("{root}al"));
    until(&mut dom, "the folder", |dom| control(dom, "Open folder alpha").is_some()).await;
    click_control(&mut dom, "Open folder alpha");
    until(&mut dom, "inside", |dom| control(dom, "Open folder inner").is_some()).await;
    assert_eq!(control(&dom, "Project directory on the server").and_then(|(_, value)| value).as_deref(), Some(format!("{root}alpha/").as_str()));
    // And back up.
    click_control(&mut dom, "Up one level");
    until(&mut dom, "outside", |dom| control(dom, "Open folder beta").is_some()).await;
    assert_eq!(control(&dom, "Project directory on the server").and_then(|(_, value)| value).as_deref(), Some(root.as_str()));
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_connection_without_the_filesystem_grant_is_told_it_cannot_browse_and_asks_for_nothing() {
    let scopes = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate];
    let (fixture, mut dom, _state) = open_dialog(&scopes).await;
    input_control(&mut dom, "Project directory on the server", "/");
    until(&mut dom, "the line", |dom| rendered_text(dom).contains("This connection cannot browse host folders.")).await;
    assert!(control(&dom, "Folders on the server").is_none());
    fixture.finish(dom).await;
}
