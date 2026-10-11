//! Context menus through the mounted UI: the menu drawn in the page (the web renderer of `context_menu::show`) with its submenus,
//! options, headers and dismissals; what a link and a pull request's number offer and do; the menu of a thread row; and the menu
//! of a tab of the file panel over a real workspace and server. The system's own menu (the desktop renderer) is checked in a
//! real window; what it is built from is `t3_client::context_menu`'s tests.
use super::diff_tests::{Fixture, open_thread, seed, until, wait_for_rendered_text};
use super::*;
use crate::context_menu::ContextMenuHost;
use crate::diff_panel::Gate;
use crate::runtime::keys_tests::key_on;
use crate::runtime::transport_tests::{click_control, click_when_enabled, control, control_attribute, mouse_control, rendered_text};
use std::{cell::RefCell, task::Poll};
use t3_client::context_menu::Item;

/// A page that records what it was asked to run in the browser (the clipboard, a new tab): the one seam this needs.
struct Eval(&'static str);
impl document::Evaluator for Eval {
    fn send(&self, _: Value) -> Result<(), document::EvalError> {
        Ok(())
    }
    fn poll_recv(&mut self, _: &mut std::task::Context<'_>) -> Poll<Result<Value, document::EvalError>> {
        Poll::Pending
    }
    fn poll_join(&mut self, _: &mut std::task::Context<'_>) -> Poll<Result<Value, document::EvalError>> {
        Poll::Pending
    }
}
struct Recorder {
    owner: dioxus::signals::Owner,
    scripts: Rc<RefCell<Vec<String>>>,
}
impl document::Document for Recorder {
    fn eval(&self, script: String) -> document::Eval {
        self.scripts.borrow_mut().push(script);
        document::Eval::new(self.owner.insert(Box::new(Eval("")) as Box<dyn document::Evaluator>))
    }
}
fn clipboard_writes(scripts: &Rc<RefCell<Vec<String>>>) -> Vec<String> {
    scripts.borrow().iter().filter_map(|script| script.split_once("navigator.clipboard?.writeText(").map(|(_, rest)| rest.trim_end_matches(");").to_owned())).collect()
}

/// Runs something with the page's state when the button "Run" is clicked.
#[derive(Clone)]
struct Run(Rc<dyn Fn(Store<UiModel>)>);
impl PartialEq for Run {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}
fn harness(run: Run) -> Element {
    let state = use_store(UiModel::default);
    rsx! { ContextMenuHost {} button { "aria-label": "Run", onclick: move |_| (run.0)(state), "Run" } }
}
fn mount(run: impl Fn(Store<UiModel>) + 'static) -> (VirtualDom, Rc<RefCell<Vec<String>>>) {
    let mut dom = VirtualDom::new_with_props(harness, Run(Rc::new(run)));
    let scripts = Rc::new(RefCell::new(vec![]));
    dom.provide_root_context(Rc::new(Recorder { owner: dioxus::signals::Owner::default(), scripts: scripts.clone() }) as Rc<dyn document::Document>);
    dom.rebuild_in_place();
    (dom, scripts)
}
fn attribute(dom: &VirtualDom, label: &str, name: &str) -> Option<String> {
    control_attribute(dom, label, name).and_then(|(_, value)| value)
}

fn sample() -> Vec<Item> {
    vec![
        Item::header("Section"),
        Item::new("plain", "Plain"),
        Item::new("a", "Option A").checked(true),
        Item::new("b", "Option B").checked(false),
        Item::new("off", "Switched off").disabled(true),
        Item::new("more", "More").children(vec![Item::new("one", "Child one"), Item::new("two", "Child two").separated()]),
        Item::new("delete", "Remove it").destructive().separated(),
    ]
}

#[tokio::test(flavor = "current_thread")]
async fn the_page_menu_marks_options_opens_submenus_and_answers_with_the_leaf_chosen() {
    let answers = Rc::new(RefCell::new(Vec::<Option<String>>::new()));
    let (mut dom, _scripts) = mount({
        let answers = answers.clone();
        move |_| {
            let answers = answers.clone();
            spawn(async move {
                let answer = crate::context_menu::show("Sample menu", sample(), Some((40.0, 60.0))).await;
                answers.borrow_mut().push(answer);
            });
        }
    });
    assert!(control(&dom, "Sample menu").is_none(), "nothing is drawn until a menu is shown");
    click_control(&mut dom, "Run");
    until(&mut dom, "the menu", |dom| control(dom, "Sample menu").is_some()).await;
    // Options carry their state; a choice that cannot be made is switched off; the destructive one looks it.
    assert_eq!(attribute(&dom, "Option A", "role").as_deref(), Some("menuitemradio"));
    assert_eq!(attribute(&dom, "Option A", "aria-checked").as_deref(), Some("true"));
    assert_eq!(attribute(&dom, "Option B", "aria-checked").as_deref(), Some("false"));
    assert_eq!(attribute(&dom, "Plain", "role").as_deref(), Some("menuitem"));
    assert_eq!(attribute(&dom, "Switched off", "disabled").as_deref(), Some("true"));
    assert!(attribute(&dom, "Remove it", "class").unwrap_or_default().contains("destructive"));
    assert!(rendered_text(&dom).contains("Section"), "a header is drawn on the page");
    // The submenu is closed until its item is chosen, and choosing it chooses nothing.
    assert!(control(&dom, "Child one").is_none());
    assert_eq!(attribute(&dom, "More", "aria-expanded").as_deref(), Some("false"));
    click_control(&mut dom, "More");
    until(&mut dom, "the submenu", |dom| control(dom, "Child two").is_some()).await;
    assert_eq!(attribute(&dom, "More", "aria-expanded").as_deref(), Some("true"));
    assert!(answers.borrow().is_empty());
    // Pointing at an item of the menu itself closes the submenu again.
    mouse_control(&mut dom, "Plain", "mouseenter", false, false, false);
    until(&mut dom, "the submenu closed", |dom| control(dom, "Child two").is_none()).await;
    click_control(&mut dom, "More");
    until(&mut dom, "the submenu", |dom| control(dom, "Child two").is_some()).await;
    click_control(&mut dom, "Child two");
    until(&mut dom, "the answer", |_| !answers.borrow().is_empty()).await;
    assert_eq!(answers.borrow().as_slice(), [Some("two".to_owned())]);
    assert!(control(&dom, "Sample menu").is_none() && control(&dom, "Child two").is_none(), "an answer closes every level");
}

#[tokio::test(flavor = "current_thread")]
async fn escape_ends_a_menu_with_no_answer_and_a_menu_shown_over_another_ends_the_first() {
    let answers = Rc::new(RefCell::new(Vec::<(&'static str, Option<String>)>::new()));
    let (mut dom, _scripts) = mount({
        let answers = answers.clone();
        let shown = Rc::new(std::cell::Cell::new(0));
        move |_| {
            let (answers, number) = (answers.clone(), shown.get());
            shown.set(number + 1);
            spawn(async move {
                let (name, label) = if number == 0 { ("first", "First menu") } else { ("second", "Second menu") };
                let answer = crate::context_menu::show(label, vec![Item::new(name, "Choose")], None).await;
                answers.borrow_mut().push((name, answer));
            });
        }
    });
    click_control(&mut dom, "Run");
    until(&mut dom, "the first menu", |dom| control(dom, "First menu").is_some()).await;
    key_on(&mut dom, "First menu", Key::Escape);
    until(&mut dom, "dismissed", |_| answers.borrow().len() == 1).await;
    assert_eq!(answers.borrow()[0], ("first", None));
    assert!(control(&dom, "First menu").is_none());
    // A second menu while one is open answers the open one with no choice and takes its place.
    click_control(&mut dom, "Run");
    until(&mut dom, "the second menu", |dom| control(dom, "Second menu").is_some()).await;
    answers.borrow_mut().clear();
    click_control(&mut dom, "Run");
    until(&mut dom, "the one open ended", |_| !answers.borrow().is_empty()).await;
    assert_eq!(answers.borrow()[0], ("second", None));
    until(&mut dom, "a menu again", |dom| control(dom, "Second menu").is_some()).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_link_offers_the_system_browser_and_a_copy_and_a_page_that_is_not_the_web_offers_nothing() {
    let (mut dom, scripts) = mount(|state| {
        spawn(async move { crate::link_menus::external_link(state, None, None, "https://example.com/docs?a=1".to_owned(), (5.0, 5.0)).await });
    });
    click_control(&mut dom, "Run");
    until(&mut dom, "the link menu", |dom| control(dom, "Link actions").is_some()).await;
    // The integrated browser is not part of this app, so it is not offered; nothing about a thread without one.
    assert!(control(&dom, "Open in integrated browser").is_none() && control(&dom, "Link to thread").is_none());
    assert!(control(&dom, "Open in system browser").is_some());
    click_control(&mut dom, "Copy Link");
    until(&mut dom, "copied", |_| !clipboard_writes(&scripts).is_empty()).await;
    assert_eq!(clipboard_writes(&scripts), ["\"https://example.com/docs?a=1\""]);
    click_control(&mut dom, "Run");
    until(&mut dom, "the link menu", |dom| control(dom, "Link actions").is_some()).await;
    click_control(&mut dom, "Open in system browser");
    until(&mut dom, "opened", |_| scripts.borrow().iter().any(|script| script.contains("window.open"))).await;
    assert!(scripts.borrow().iter().any(|script| script.contains("window.open(\"https://example.com/docs?a=1\",'_blank','noopener')")), "{:?}", scripts.borrow());
    // Only a web page has the menu: a mailto link or a script is left to the page.
    assert!(crate::link_menus::web_link("mailto:someone@example.com").is_none() && crate::link_menus::web_link("javascript:alert(1)").is_none());
    assert!(crate::link_menus::web_link("//example.com/x").is_some());
}

#[tokio::test(flavor = "current_thread")]
async fn a_pull_requests_number_offers_the_copy_before_opening_it_on_its_host() {
    let (mut dom, scripts) = mount(|state| {
        spawn(async move { crate::link_menus::pull_request_number(state, "https://gitlab.example/acme/lab/-/merge_requests/7".to_owned(), Some("gitlab".to_owned()), (1.0, 1.0)).await });
    });
    click_control(&mut dom, "Run");
    until(&mut dom, "the menu", |dom| control(dom, "Pull request link").is_some()).await;
    // The host is named for where the reader will land.
    assert!(control(&dom, "Copy link").is_some() && control(&dom, "Open on GitLab").is_some());
    click_control(&mut dom, "Copy link");
    until(&mut dom, "copied", |_| !clipboard_writes(&scripts).is_empty()).await;
    assert_eq!(clipboard_writes(&scripts), ["\"https://gitlab.example/acme/lab/-/merge_requests/7\""]);
}

#[tokio::test(flavor = "current_thread")]
async fn a_picture_in_a_message_offers_its_paths_the_viewer_saving_and_copying_the_picture() {
    let opened = Rc::new(std::cell::Cell::new(false));
    let (mut dom, scripts) = mount({
        let opened = opened.clone();
        move |state| {
            let opened = opened.clone();
            let open_file = Callback::new(move |()| opened.set(true));
            let reference = t3_client::context_menu::MediaReference::File { path: "/work/shots/a.png".into(), relative_path: Some("shots/a.png".into()) };
            spawn(async move { crate::link_menus::media(state, "image", "a.png".into(), Some("https://assets.invalid/a.png?sig=1".into()), Some(reference), Some(open_file), (2.0, 2.0)).await });
        }
    });
    click_control(&mut dom, "Run");
    until(&mut dom, "the menu", |dom| control(dom, "Media actions").is_some()).await;
    for label in ["Copy full path", "Copy relative path", "Open in file viewer", "Save image", "Copy image"] {
        assert!(control(&dom, label).is_some(), "{label}");
    }
    click_control(&mut dom, "Copy relative path");
    until(&mut dom, "copied", |_| !clipboard_writes(&scripts).is_empty()).await;
    assert_eq!(clipboard_writes(&scripts), ["\"shots/a.png\""]);
    click_control(&mut dom, "Run");
    until(&mut dom, "the menu", |dom| control(dom, "Media actions").is_some()).await;
    click_control(&mut dom, "Copy image");
    until(&mut dom, "the picture is fetched and put on the clipboard", |_| scripts.borrow().iter().any(|script| script.contains("ClipboardItem"))).await;
    assert!(scripts.borrow().iter().any(|script| script.contains("fetch(\"https://assets.invalid/a.png?sig=1\")")));
    click_control(&mut dom, "Run");
    until(&mut dom, "the menu", |dom| control(dom, "Media actions").is_some()).await;
    click_control(&mut dom, "Open in file viewer");
    until(&mut dom, "the viewer asked for", |_| opened.get()).await;
}

const SCOPES: [AuthEnvironmentScope; 3] = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::FilesystemRead];

#[tokio::test(flavor = "current_thread")]
async fn a_thread_rows_menu_groups_what_the_originals_does_and_a_submenu_choice_acts() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    until(&mut dom, "the row", |dom| control(dom, "Thread thread-a").is_some()).await;
    mouse_control(&mut dom, "Thread thread-a", "contextmenu", false, false, false);
    until(&mut dom, "the menu", |dom| control(dom, "Thread actions").is_some()).await;
    // Copying is one item with the three things to copy inside, as the original groups them.
    assert!(control(&dom, "Path").is_none() && control(&dom, "Thread ID").is_none());
    assert!(control(&dom, "Copy thread ID").is_none());
    click_control(&mut dom, "Copy");
    until(&mut dom, "the copy submenu", |dom| control(dom, "Thread ID").is_some()).await;
    assert!(control(&dom, "Path").is_some());
    // Delete is its own destructive item and asks first.
    assert!(attribute(&dom, "Delete", "class").unwrap_or_default().contains("destructive"));
    click_control(&mut dom, "Delete");
    wait_for_rendered_text(&mut dom, "Delete thread \"thread-a\"?").await;
    assert!(control(&dom, "Thread actions").is_none(), "choosing closes the menu");
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_file_tabs_menu_closes_the_others_or_the_ones_to_its_right() {
    let fixture = Fixture::start(false).await;
    std::fs::create_dir_all(fixture.workspace.join("src")).unwrap();
    for name in ["one", "two", "three"] {
        std::fs::write(fixture.workspace.join(format!("src/{name}.txt")), format!("contents of {name}\n")).unwrap();
    }
    {
        let (mut dom, props) = fixture.client(Gate::default(), &SCOPES);
        seed(&mut dom, &props, &fixture.workspace).await;
    }
    let (mut dom, props) = fixture.client(Gate::default(), &SCOPES);
    let state = props.state.borrow().unwrap();
    until(&mut dom, "shell", |_| state.peek().shell.snapshot.as_ref().is_some_and(|snapshot| snapshot.threads.len() == 2) && state.peek().typed_config.is_some()).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    let cwd = fixture.workspace.to_string_lossy().into_owned();
    for name in ["one", "two", "three"] {
        crate::file_panel::open(state, Some("thread-a".to_owned()), cwd.clone(), format!("src/{name}.txt"), None);
        wait_for_rendered_text(&mut dom, &format!("contents of {name}")).await;
    }
    let tabs = crate::right_panel::file_tabs;
    assert_eq!(tabs(state), ["src/one.txt", "src/two.txt", "src/three.txt"]);
    // The last tab has nothing to its right, and the only tab nothing beside it: those are switched off, not left out.
    mouse_control(&mut dom, "three.txt", "contextmenu", false, false, false);
    until(&mut dom, "the tab menu", |dom| control(dom, "Tab actions").is_some()).await;
    assert_eq!(attribute(&dom, "Close to the right", "disabled").as_deref(), Some("true"));
    assert_eq!(attribute(&dom, "Close others", "disabled").as_deref(), Some("false"));
    key_on(&mut dom, "Tab actions", Key::Escape);
    until(&mut dom, "dismissed", |dom| control(dom, "Tab actions").is_none()).await;
    assert_eq!(tabs(state).len(), 3, "dismissing closes nothing");
    // Closing to the right of the middle one leaves two; the tab shown went, so the one the menu was opened on takes its place.
    mouse_control(&mut dom, "two.txt", "contextmenu", false, false, false);
    until(&mut dom, "the tab menu", |dom| control(dom, "Tab actions").is_some()).await;
    click_control(&mut dom, "Close to the right");
    until(&mut dom, "the tab to its right gone", |_| tabs(state) == ["src/one.txt", "src/two.txt"]).await;
    wait_for_rendered_text(&mut dom, "contents of two").await;
    assert_eq!(crate::right_panel::shown_file(state).map(|(path, _)| path).as_deref(), Some("src/two.txt"));
    // Closing the others leaves the one chosen, shown; the panel keeps its tab bar.
    mouse_control(&mut dom, "one.txt", "contextmenu", false, false, false);
    until(&mut dom, "the tab menu", |dom| control(dom, "Tab actions").is_some()).await;
    click_when_enabled(&mut dom, "Close others").await;
    until(&mut dom, "the others gone", |_| tabs(state) == ["src/one.txt"]).await;
    wait_for_rendered_text(&mut dom, "contents of one").await;
    assert_eq!(crate::right_panel::shown_file(state).map(|(path, _)| path).as_deref(), Some("src/one.txt"));
    fixture.finish(dom).await;
}
