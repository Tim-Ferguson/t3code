//! The Projects page's icon row through the mounted UI and the real native server over a real workspace: the image files of the project
//! are searched, the one chosen becomes the project's icon (and the server keeps it), the desktop app's system dialog is offered when
//! it can be and its answer is used, and the reset puts the automatic icon back.
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::diff_tests::{Fixture, rendered_text, seed, until, wait_for_rendered_text};
use crate::runtime::transport_tests::{click_when_enabled, control, input_control};
use std::cell::RefCell;

const SCOPES: [AuthEnvironmentScope; 3] = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::FilesystemRead];
const PICK_FILE: &str = "Choose a project icon file";
const SEARCH: &str = "Search image files\u{2026}";
const RESET: &str = "Reset project icon to default";

fn favicon_of_project(state: Store<UiModel>) -> Option<String> {
    let shell = state.shell();
    let shell = shell.read();
    shell.snapshot.as_ref()?.projects.iter().find(|project| project.id.as_str() == "project")?.favicon_path().map(str::to_owned)
}

async fn open_page(fixture: &Fixture, setup: impl FnOnce(&mut VirtualDom)) -> (VirtualDom, Store<UiModel>) {
    fixture.write("logo.svg", "<svg/>");
    std::fs::create_dir_all(fixture.workspace.join("assets")).unwrap();
    std::fs::write(fixture.workspace.join("assets/icon.svg"), "<svg/>").unwrap();
    std::fs::create_dir_all(fixture.workspace.join("docs")).unwrap();
    std::fs::write(fixture.workspace.join("docs/notes.txt"), "not an image").unwrap();
    let (mut dom, props) = fixture.client_with(Gate::default(), &SCOPES, setup);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "config", |_| state.peek().typed_config.is_some()).await;
    crate::projects_page::open(state, "project");
    until(&mut dom, "the row", |dom| control(dom, PICK_FILE).is_some()).await;
    (dom, state)
}

#[tokio::test(flavor = "current_thread")]
async fn an_image_of_the_project_is_chosen_for_its_icon_and_the_reset_puts_the_automatic_one_back() {
    let fixture = Fixture::start(false).await;
    let (mut dom, state) = open_page(&fixture, |_| {}).await;
    assert!(rendered_text(&dom).contains("Project icon") && rendered_text(&dom).contains("Automatic"));
    assert!(control(&dom, RESET).is_none(), "nothing to reset yet");

    click_when_enabled(&mut dom, PICK_FILE).await;
    until(&mut dom, "the picker", |dom| control(dom, SEARCH).is_some()).await;
    // Images only, by the server's own filter: the text file and the folder are not offered.
    until(&mut dom, "the images", |dom| control(dom, "logo.svg").is_some() && control(dom, "assets/icon.svg").is_some()).await;
    assert!(control(&dom, "docs/notes.txt").is_none());
    assert!(rendered_text(&dom).find("Open in").is_none(), "a browser has no system dialog");

    // Typing narrows the list to what matches.
    input_control(&mut dom, SEARCH, "icon");
    until(&mut dom, "the narrowed list", |dom| control(dom, "assets/icon.svg").is_some() && control(dom, "logo.svg").is_none()).await;
    input_control(&mut dom, SEARCH, "zzzzqqqq");
    wait_for_rendered_text(&mut dom, "No matching image files.").await;
    input_control(&mut dom, SEARCH, "icon");
    until(&mut dom, "the list again", |dom| control(dom, "assets/icon.svg").is_some()).await;

    click_when_enabled(&mut dom, "assets/icon.svg").await;
    until(&mut dom, "the choice saved", |_| favicon_of_project(state).as_deref() == Some("assets/icon.svg")).await;
    until(&mut dom, "the picker closed", |dom| control(dom, SEARCH).is_none()).await;
    wait_for_rendered_text(&mut dom, "assets/icon.svg").await;
    assert!(control(&dom, RESET).is_some());

    click_when_enabled(&mut dom, RESET).await;
    until(&mut dom, "the reset saved", |_| favicon_of_project(state).is_none()).await;
    wait_for_rendered_text(&mut dom, "Automatic").await;
    assert!(control(&dom, RESET).is_none());
    fixture.finish(dom).await;
}

/// A system dialog that answers what the test says, and records that it was asked.
struct Dialog {
    offered: bool,
    answer: RefCell<Option<Result<Option<String>, String>>>,
    asked: RefCell<Vec<String>>,
}
impl Picker for Dialog {
    fn offered(&self, _: &TransportHandle, _: &str) -> bool {
        self.offered
    }
    fn manager_name(&self) -> &'static str {
        "Finder"
    }
    fn pick(&self, workspace_root: &str) -> LocalBoxFuture<'static, Result<Option<String>, String>> {
        self.asked.borrow_mut().push(workspace_root.to_owned());
        Box::pin(std::future::ready(self.answer.borrow_mut().take().unwrap_or(Ok(None))))
    }
}

#[tokio::test(flavor = "current_thread")]
async fn the_system_dialog_is_offered_in_the_desktop_app_and_its_file_becomes_the_icon() {
    let fixture = Fixture::start(false).await;
    let dialog = Rc::new(Dialog { offered: true, answer: RefCell::new(None), asked: RefCell::default() });
    let (mut dom, state) = open_page(&fixture, {
        let dialog = dialog.clone();
        move |dom| dom.provide_root_context(PickerHandle(dialog))
    })
    .await;
    click_when_enabled(&mut dom, PICK_FILE).await;
    until(&mut dom, "the picker", |dom| control(dom, "Open in Finder").is_some()).await;

    // Cancelling the system dialog leaves the picker as it was.
    click_when_enabled(&mut dom, "Open in Finder").await;
    until(&mut dom, "the dialog asked", |_| dialog.asked.borrow().len() == 1).await;
    assert_eq!(dialog.asked.borrow()[0], fixture.workspace.to_string_lossy());
    assert!(control(&dom, SEARCH).is_some() && favicon_of_project(state).is_none());

    // A failure is said, and the picker stays.
    *dialog.answer.borrow_mut() = Some(Err("The dialog could not open.".to_owned()));
    click_when_enabled(&mut dom, "Open in Finder").await;
    wait_for_rendered_text(&mut dom, "Could not open image picker").await;
    assert!(control(&dom, SEARCH).is_some());

    // A file of this computer is the icon, by its path.
    *dialog.answer.borrow_mut() = Some(Ok(Some("/Users/me/Pictures/logo.png".to_owned())));
    click_when_enabled(&mut dom, "Open in Finder").await;
    until(&mut dom, "saved", |_| favicon_of_project(state).as_deref() == Some("/Users/me/Pictures/logo.png")).await;
    until(&mut dom, "closed", |dom| control(dom, SEARCH).is_none()).await;
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_project_the_system_dialog_cannot_serve_gets_no_such_button() {
    let fixture = Fixture::start(false).await;
    let dialog = Rc::new(Dialog { offered: false, answer: RefCell::new(None), asked: RefCell::default() });
    let (mut dom, _state) = open_page(&fixture, move |dom| dom.provide_root_context(PickerHandle(dialog))).await;
    click_when_enabled(&mut dom, PICK_FILE).await;
    until(&mut dom, "the images", |dom| control(dom, "logo.svg").is_some()).await;
    assert!(control(&dom, "Open in Finder").is_none());
    fixture.finish(dom).await;
}

#[test]
fn the_row_describes_the_icon_as_the_original_does() {
    use t3_contracts::{ProjectIcon, ProjectIconColor};
    assert_eq!(describe(None, None), "Automatic");
    assert_eq!(describe(None, Some("assets/icon.svg")), "assets/icon.svg");
    assert_eq!(describe(Some(&ProjectIcon::Emoji { emoji: "\u{1f680}".into() }), Some("ignored.png")), "\u{1f680}", "an icon of the project's own wins over the file");
    assert_eq!(describe(Some(&ProjectIcon::Monogram { text: "AB".into(), color: ProjectIconColor::Teal }), None), "AB \u{b7} teal");
    assert_eq!(describe(Some(&ProjectIcon::Lucide { name: "folder-code".into(), color: ProjectIconColor::Gray }), None), "folder-code \u{b7} gray");
    assert_eq!(search_input("/w", " logo ")["imageOnly"], true);
    assert_eq!(search_input("/w", "logo")["kind"], "file");
    assert_eq!(update_input("p", Some("a.png"))["projectIcon"], Value::Null);
    assert_eq!(update_input("p", None)["faviconPath"], Value::Null);
    assert_eq!(empty_message("", None, true), "Indexing project files\u{2026}");
    assert_eq!(empty_message("x", None, true), "Searching project files\u{2026}");
    assert_eq!(empty_message("", None, false), "No image files found.");
    assert_eq!(empty_message("x", Some("It broke."), false), "It broke.");
}
