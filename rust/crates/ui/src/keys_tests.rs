//! Shortcuts, the command palette, the Keybindings page, thread search and project creation
//! through the mounted UI against the real native server: a real keybindings file, a real search
//! over real turns, real project folders. Keys reach the app the way the page's listener sends
//! them. Waits are on state or text that only the awaited step produces.
use super::diff_tests::{Fixture, Harness, open_thread, rendered_text, seed, turn, until, wait_for_rendered_text};
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::transport_tests::{all_controls, click_control, control, control_attribute, input_control};
use crate::{keys, search};
use t3_client::keybindings::{Dom, KeyEvent};

pub(crate) const PALETTE: &str = "Search commands, projects, and threads...";
const OPERATE: [AuthEnvironmentScope; 3] = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::SettingsWrite];

/// A chord with `mod` as this machine resolves it.
pub(crate) fn chord(key: &str, shift: bool, alt: bool) -> KeyEvent {
    let mac = keys::platform().is_mac();
    KeyEvent { key: key.into(), code: format!("Key{}", key.to_uppercase()), meta: mac, ctrl: !mac, shift, alt, ..Default::default() }
}
pub(crate) fn typed(key: &str) -> KeyEvent {
    KeyEvent { key: key.into(), code: format!("Key{}", key.to_uppercase()), ..Default::default() }
}
/// Presses a key and waits for the app to have decided what to do with it: the command it ran, or None.
pub(crate) async fn press(dom: &mut VirtualDom, event: KeyEvent, focus: Dom) -> Option<String> {
    let before = keys::HANDLED.with(|handled| handled.borrow().len());
    keys::press(event, focus);
    poll_until(dom, "the key to be handled", |_| keys::HANDLED.with(|handled| handled.borrow().len()) > before).await;
    keys::HANDLED.with(|handled| handled.borrow().last().cloned().unwrap())
}
/// Waits for a step that may repaint nothing (a key the app leaves alone, an answer held on its
/// way to the screen), so there may be no work to wake on: the runtime is polled once per turn.
pub(crate) async fn poll_until(dom: &mut VirtualDom, what: &str, ready: impl Fn(&VirtualDom) -> bool) {
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        while !ready(dom) {
            tokio::select! { biased; _ = dom.wait_for_work() => {}, _ = tokio::task::yield_now() => {} }
            dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}
fn in_composer() -> Dom {
    Dom { editable_focus: true, composer_focus: true, ..Default::default() }
}
fn label_for(command: &str) -> String {
    t3_client::keybindings::shortcut_label(&t3_client::keybindings::merge_with_defaults(vec![]), command, keys::platform(), &t3_client::keybindings::Context::base(true)).unwrap()
}

/// Replaces the environment's keybindings file the way an editor save does.
fn write_keybindings(fixture: &Fixture, contents: &str) {
    let path = fixture.directory.path().join("keybindings.json");
    let staging = path.with_extension("tmp");
    std::fs::write(&staging, contents).unwrap();
    std::fs::rename(staging, path).unwrap();
}
fn has_rule(state: Store<UiModel>, command: &str, key: &str) -> bool {
    state.typed_config().peek().as_ref().is_some_and(|config| config.keybindings.0.iter().any(|rule| rule.command.as_str() == command && rule.shortcut.key.0.as_str() == key))
}

async fn connect(fixture: &Fixture, scopes: &[AuthEnvironmentScope]) -> (VirtualDom, Harness, Store<UiModel>) {
    let (mut dom, props) = fixture.client(Gate::default(), scopes);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "config", |_| state.peek().typed_config.is_some() && has_rule(state, "diff.toggle", "d")).await;
    (dom, props, state)
}
fn open_palette_text(dom: &VirtualDom) -> bool {
    control(dom, PALETTE).is_some()
}
fn diff_open(dom: &VirtualDom) -> bool {
    control_attribute(dom, "Diff", "aria-pressed").and_then(|(_, value)| value).as_deref() == Some("true")
}

#[tokio::test(flavor = "current_thread")]
async fn a_default_shortcut_runs_its_command_and_a_key_nothing_names_is_left_alone() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props, state) = connect(&fixture, &OPERATE).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    assert!(!diff_open(&dom));
    assert_eq!(press(&mut dom, chord("d", false, false), Dom::default()).await.as_deref(), Some("diff.toggle"));
    until(&mut dom, "diff open", diff_open).await;
    // The same shortcut toggles it closed.
    assert_eq!(press(&mut dom, chord("d", false, false), Dom::default()).await.as_deref(), Some("diff.toggle"));
    until(&mut dom, "diff closed", |dom| !diff_open(dom)).await;
    // Chords no rule names, and chords of commands this app does not have, do nothing.
    for (event, why) in [(chord("q", true, true), "no rule"), (chord("p", false, false), "file picker is not here"), (chord("k", true, false), "copying a pull request number is not here"), (typed("q"), "plain letter")] {
        assert_eq!(press(&mut dom, event, Dom::default()).await, None, "{why}");
    }
    assert!(!diff_open(&dom));
    // The terminal owns its chords: mod+d there is the terminal's split, which the app does not claim.
    assert_eq!(press(&mut dom, chord("d", false, false), Dom { terminal_focus: true, ..Default::default() }).await, None);
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_rule_in_the_file_overrides_a_default_and_a_change_to_the_file_applies_without_reload() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props, state) = connect(&fixture, &OPERATE).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    write_keybindings(&fixture, r#"[{"key":"mod+shift+y","command":"diff.toggle"}]"#);
    until(&mut dom, "the file's rule", |_| has_rule(state, "diff.toggle", "y")).await;
    // The default chord no longer does it; the file's does.
    assert_eq!(press(&mut dom, chord("d", false, false), Dom::default()).await, None);
    assert_eq!(press(&mut dom, chord("y", true, false), Dom::default()).await.as_deref(), Some("diff.toggle"));
    until(&mut dom, "diff open", diff_open).await;
    // Another edit moves it again, on the same connection.
    write_keybindings(&fixture, r#"[{"key":"mod+alt+u","command":"diff.toggle"}]"#);
    until(&mut dom, "the next rule", |_| has_rule(state, "diff.toggle", "u")).await;
    assert_eq!(press(&mut dom, chord("y", true, false), Dom::default()).await, None);
    assert_eq!(press(&mut dom, chord("u", false, true), Dom::default()).await.as_deref(), Some("diff.toggle"));
    until(&mut dom, "diff closed", |dom| !diff_open(dom)).await;
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_shortcut_is_ignored_while_typing_in_the_composer_unless_its_rule_allows_it() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props, state) = connect(&fixture, &OPERATE).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    write_keybindings(&fixture, r#"[{"key":"x","command":"diff.toggle"},{"key":"mod+shift+y","command":"sidebar.toggle","when":"composerFocus"}]"#);
    until(&mut dom, "the file's rules", |_| has_rule(state, "diff.toggle", "x") && has_rule(state, "sidebar.toggle", "y")).await;
    // Typing the letter in the message box is typing.
    assert_eq!(press(&mut dom, typed("x"), in_composer()).await, None);
    assert!(!diff_open(&dom));
    // A Control chord in the same box is a command, as in the original.
    assert_eq!(press(&mut dom, chord("y", true, false), in_composer()).await.as_deref(), Some("sidebar.toggle"));
    // Outside a text field the plain letter works.
    assert_eq!(press(&mut dom, typed("x"), Dom::default()).await.as_deref(), Some("diff.toggle"));
    // A rule that names the composer focus fires only there.
    assert_eq!(press(&mut dom, chord("y", true, false), Dom::default()).await, None);
    // IME composition is never a shortcut, whatever it looks like.
    assert_eq!(press(&mut dom, KeyEvent { composing: true, ..chord("y", true, false) }, in_composer()).await, None);
    assert_eq!(press(&mut dom, KeyEvent { key_code: 229, ..chord("d", false, false) }, Dom::default()).await, None);
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn the_palette_opens_filters_runs_a_command_and_closes() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props, state) = connect(&fixture, &OPERATE).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    assert!(!open_palette_text(&dom));
    assert_eq!(press(&mut dom, chord("k", false, false), Dom::default()).await.as_deref(), Some("commandPalette.toggle"));
    until(&mut dom, "palette", open_palette_text).await;
    // Sections, ordering and the hint beside a command.
    let text = rendered_text(&dom);
    assert!(text.find("Actions").unwrap() < text.find("Recent Threads").unwrap());
    assert!(text.contains(&label_for("chat.new")) && text.contains(&label_for("diff.toggle")), "{text}");
    assert!(control(&dom, "Open settings").is_some() && control(&dom, "Toggle diff panel").is_some());
    // Typing filters; a command that is not offered is not listed.
    input_control(&mut dom, PALETTE, "diff");
    assert!(control(&dom, "Toggle diff panel").is_some() && control(&dom, "Open settings").is_none());
    assert!(control(&dom, "Go to file").is_none(), "a feature this app does not have");
    input_control(&mut dom, PALETTE, "no such command");
    assert!(rendered_text(&dom).contains("No matching commands, projects, or threads."));
    input_control(&mut dom, PALETTE, ">diff");
    assert!(control(&dom, "Toggle diff panel").is_some());
    // Enter runs the highlighted command and closes the palette.
    key_on(&mut dom, PALETTE, Key::Enter);
    assert!(!open_palette_text(&dom));
    until(&mut dom, "diff open", diff_open).await;
    // Escape closes it, and the shortcut that opened it closes it.
    press(&mut dom, chord("k", false, false), Dom::default()).await;
    until(&mut dom, "palette", open_palette_text).await;
    key_on(&mut dom, PALETTE, Key::Escape);
    assert!(!open_palette_text(&dom));
    press(&mut dom, chord("k", false, false), Dom::default()).await;
    until(&mut dom, "palette", open_palette_text).await;
    assert_eq!(press(&mut dom, chord("k", false, false), Dom { dialog_open: true, ..Default::default() }).await.as_deref(), Some("commandPalette.toggle"));
    assert!(!open_palette_text(&dom));
    // While it is open the other shortcuts wait.
    press(&mut dom, chord("k", false, false), Dom::default()).await;
    until(&mut dom, "palette", open_palette_text).await;
    assert_eq!(press(&mut dom, chord("d", false, false), Dom { dialog_open: true, ..Default::default() }).await, None);
    // Add project opens the dialog the sidebar button opens.
    press(&mut dom, chord("k", false, false), Dom { dialog_open: true, ..Default::default() }).await;
    assert!(!open_palette_text(&dom));
    open_palette(&mut dom).await;
    click_control(&mut dom, "Add project");
    assert!(!open_palette_text(&dom) && rendered_text(&dom).contains("Project directory on the server"));
    // The theme shortcut opens the palette on the theme page, whose Backspace goes back.
    press(&mut dom, KeyEvent { alt: true, ..chord("a", false, false) }, Dom::default()).await;
    until(&mut dom, "theme page", |dom| control(dom, "Search...").is_some()).await;
    assert!(control(&dom, "T3 Code (default)").is_some() && control(&dom, "Open settings").is_none());
    key_on(&mut dom, "Search...", Key::Backspace);
    until(&mut dom, "root page", open_palette_text).await;
    key_on(&mut dom, PALETTE, Key::Escape);
    assert!(!open_palette_text(&dom));
    fixture.finish(dom).await;
}

/// Presses a key on the control with this label.
pub(crate) fn key_on(dom: &mut VirtualDom, label: &str, key: Key) {
    dioxus_html::set_event_converter(Box::new(dioxus_html::SerializedHtmlEventConverter));
    let (element, _) = control(dom, label).unwrap_or_else(|| panic!("missing control {label}"));
    let data = dioxus_html::SerializedKeyboardData::new(key, Code::Unidentified, Location::Standard, false, Modifiers::empty(), false);
    dom.runtime().handle_event("keydown", dioxus::dioxus_core::Event::new(Rc::new(dioxus_html::PlatformEventData::new(Box::new(data))) as Rc<dyn std::any::Any>, true), element);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
}

/// Presses a key with modifiers on the control with this label.
fn chord_on(dom: &mut VirtualDom, label: &str, key: &str, code: Code) {
    dioxus_html::set_event_converter(Box::new(dioxus_html::SerializedHtmlEventConverter));
    let (element, _) = control(dom, label).unwrap_or_else(|| panic!("missing control {label}"));
    let modifier = if keys::platform().is_mac() { Modifiers::META } else { Modifiers::CONTROL };
    let data = dioxus_html::SerializedKeyboardData::new(Key::Character(key.into()), code, Location::Standard, false, modifier, false);
    dom.runtime().handle_event("keydown", dioxus::dioxus_core::Event::new(Rc::new(dioxus_html::PlatformEventData::new(Box::new(data))) as Rc<dyn std::any::Any>, true), element);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
}
pub(crate) async fn open_palette(dom: &mut VirtualDom) {
    press(dom, chord("k", false, false), Dom::default()).await;
    until(dom, "palette", open_palette_text).await;
}

#[tokio::test(flavor = "current_thread")]
async fn commands_without_their_feature_or_their_grant_are_not_offered_and_their_shortcuts_do_nothing() {
    let fixture = Fixture::start(false).await;
    {
        // Something to look at: a project and threads, made by an operator.
        let (mut dom, props) = fixture.client(Gate::default(), &OPERATE);
        seed(&mut dom, &props, &fixture.workspace).await;
    }
    let (mut dom, props) = fixture.client(Gate::default(), &[AuthEnvironmentScope::OrchestrationRead]);
    let state = props.state.borrow().unwrap();
    until(&mut dom, "shell", |_| state.peek().shell.snapshot.as_ref().is_some_and(|snapshot| snapshot.threads.len() == 2) && has_rule(state, "diff.toggle", "d")).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    open_palette(&mut dom).await;
    assert!(control(&dom, "Toggle diff panel").is_some(), "reading is enough for the diff");
    for missing in ["New thread without a project", "Interrupt running turn", "Switch branch", "Toggle terminal", "New terminal", "Split terminal", "Split terminal vertically", "Close terminal"] {
        assert!(control(&dom, missing).is_none(), "{missing} needs a grant or a feature this connection lacks");
    }
    // "New project" is the sidebar's button alone, and it is disabled; the palette has no entry.
    assert_eq!(all_controls(&dom, "New project").len(), 1);
    assert_eq!(control_attribute(&dom, "New project", "disabled").and_then(|(_, value)| value).as_deref(), Some("true"));
    // The shortcut of a command that is not offered is not claimed either.
    press(&mut dom, chord("k", false, false), Dom::default()).await;
    until(&mut dom, "closed", |dom| !open_palette_text(dom)).await;
    assert_eq!(press(&mut dom, chord("n", false, true), Dom::default()).await, None, "no thread without a project without operate");
    // With no thread open the thread commands are not offered.
    state.active_thread().set(None);
    open_palette(&mut dom).await;
    assert!(control(&dom, "Toggle diff panel").is_none() && control(&dom, "Focus composer").is_none());
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn the_keybindings_page_records_shows_a_clash_saves_resets_and_removes() {
    let fixture = Fixture::start(false).await;
    let (mut dom, _props, state) = connect(&fixture, &OPERATE).await;
    // The page is a palette command.
    open_palette(&mut dom).await;
    input_control(&mut dom, PALETTE, "keybindings");
    click_control(&mut dom, "Settings: Keybindings");
    until(&mut dom, "the page", |dom| control(dom, "Search keybindings").is_some()).await;
    let default_label = label_for("diff.toggle");
    // Only commands this app has are listed.
    assert!(control(&dom, &format!("Edit shortcut for Diff: Toggle: {default_label}")).is_some());
    assert!(control(&dom, "Edit shortcut for Preview: Toggle: ").is_none());
    assert!(rendered_text(&dom).contains("more shortcuts in the file are for features this app does not have yet"));

    // Record mod+k for the diff: it takes the chord of the palette under the same condition.
    click_control(&mut dom, &format!("Edit shortcut for Diff: Toggle: {default_label}"));
    chord_on(&mut dom, "Keybinding for Diff: Toggle", "k", Code::KeyK);
    assert!(rendered_text(&dom).contains("Conflicts with Command Palette: Toggle"), "{}", rendered_text(&dom));
    click_control(&mut dom, "Save keybinding for Diff: Toggle");
    let recorded = t3_client::keybindings::format_shortcut_label(&t3_client::keybindings::parse_shortcut("mod+k").unwrap(), keys::platform());
    until(&mut dom, "saved", |dom| control(dom, &format!("Edit shortcut for Diff: Toggle: {recorded}")).is_some()).await;
    assert!(has_rule(state, "diff.toggle", "k") && !has_rule(state, "diff.toggle", "d"));
    assert!(rendered_text(&dom).contains("Custom"));
    // The saved rule is the live one, with no reload.
    assert_eq!(press(&mut dom, chord("d", false, false), Dom::default()).await, None);

    // Reset puts the default back, and the Custom mark goes.
    click_control(&mut dom, "Reset Diff: Toggle to default");
    until(&mut dom, "reset", |dom| control(dom, &format!("Edit shortcut for Diff: Toggle: {default_label}")).is_some()).await;
    assert!(has_rule(state, "diff.toggle", "d") && !has_rule(state, "diff.toggle", "k"));
    assert!(control(&dom, "Reset Diff: Toggle to default").is_none());

    // Adding a rule for a command, then removing it.
    click_control(&mut dom, "Add keybinding");
    crate::runtime::transport_tests::change_control(&mut dom, "Command", "chat.new");
    click_control(&mut dom, "Keybinding for Chat: New");
    chord_on(&mut dom, "Keybinding for Chat: New", "j", Code::KeyJ);
    click_control(&mut dom, "Save new keybinding");
    until(&mut dom, "added", |_| has_rule(state, "chat.new", "j")).await;
    until(&mut dom, "removable", |dom| control(dom, "Remove shortcut for Chat: New").is_some()).await;
    click_control(&mut dom, "Remove shortcut for Chat: New");
    until(&mut dom, "removed", |_| !has_rule(state, "chat.new", "j")).await;
    assert!(has_rule(state, "chat.new", "n"), "the defaults stay");
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn problems_in_the_file_are_shown_with_its_path_and_a_connection_without_write_access_cannot_change_anything() {
    let fixture = Fixture::start(false).await;
    let (mut dom, _props, state) = connect(&fixture, &OPERATE).await;
    state.view().set(View::Keybindings);
    until(&mut dom, "the page", |dom| control(dom, "Search keybindings").is_some()).await;
    assert!(!rendered_text(&dom).contains("The keybindings file has problems"));
    write_keybindings(&fixture, r#"{"not":"a list"}"#);
    until(&mut dom, "the problem", |dom| rendered_text(dom).contains("The keybindings file has problems")).await;
    let path = state.typed_config().peek().as_ref().unwrap().keybindings_config_path.to_string();
    assert!(path.ends_with("keybindings.json") && rendered_text(&dom).contains(&path), "the file is named");
    until(&mut dom, "the notice", |dom| rendered_text(dom).contains("Invalid keybindings configuration")).await;
    // A reader sees the same page, read-only.
    let (mut reader, props) = fixture.client(Gate::default(), &[AuthEnvironmentScope::OrchestrationRead]);
    let state = props.state.borrow().unwrap();
    until(&mut reader, "config", |_| state.peek().typed_config.is_some()).await;
    state.view().set(View::Keybindings);
    until(&mut reader, "the page", |dom| control(dom, "Search keybindings").is_some()).await;
    assert!(rendered_text(&reader).contains(crate::keybinding_page::READ_ONLY));
    assert_eq!(control_attribute(&reader, "Add keybinding", "disabled").and_then(|(_, value)| value).as_deref(), Some("true"));
    fixture.finish(dom).await;
}

/// Two threads, with a turn run in `thread-a`, and `thread-b` open.
async fn with_a_conversation(fixture: &Fixture) -> (VirtualDom, Harness, Store<UiModel>) {
    let (mut dom, props, state) = connect(fixture, &OPERATE).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    turn(&mut dom, &props, state, 1).await;
    open_thread(&mut dom, &props, state, "thread-b").await;
    (dom, props, state)
}
fn requests() -> Vec<String> {
    search::REQUESTS.with(|requests| requests.borrow().clone())
}

#[tokio::test(flavor = "current_thread")]
async fn sidebar_search_finds_titles_at_once_and_messages_after_a_pause_and_opens_the_result() {
    let fixture = Fixture::start(false).await;
    let (mut dom, _props, state) = with_a_conversation(&fixture).await;
    assert!(control(&dom, "Clear thread search").is_none());
    // A title matches as it is typed; one character never goes to the server.
    input_control(&mut dom, "Search threads", "b");
    assert!(control(&dom, "thread-b").is_some() && control(&dom, "thread-a").is_none());
    assert!(requests().is_empty());
    // Messages are found by the server, and shown with who said it and the matched words marked.
    input_control(&mut dom, "Search threads", "Do the work");
    // The first message gave thread-a the title "Do the work 1", so the title matches at once too.
    assert!(control(&dom, "Do the work 1").is_some());
    wait_for_rendered_text(&mut dom, "You: Do the work 1").await;
    assert_eq!(requests(), ["Do the work"], "the one-character query was never sent");
    assert!(control(&dom, "Do the work 1").is_some() && control(&dom, "thread-b").is_none());
    // Opening a result goes to its thread and clears the search.
    click_control(&mut dom, "Do the work 1");
    assert_eq!(state.active_thread().peek().as_deref(), Some("thread-a"));
    assert!(control(&dom, "Clear thread search").is_none());
    // Nothing found says so once the server has answered, and Escape clears.
    input_control(&mut dom, "Search threads", "zzzz");
    wait_for_rendered_text(&mut dom, t3_client::thread_search::STATUS_NONE).await;
    key_on(&mut dom, "Search threads", Key::Escape);
    assert!(control(&dom, "Clear thread search").is_none());
    // The keyboard moves through results and Enter opens the highlighted one.
    input_control(&mut dom, "Search threads", "thread");
    assert!(control(&dom, "thread-b").is_some());
    key_on(&mut dom, "Search threads", Key::ArrowDown);
    key_on(&mut dom, "Search threads", Key::Enter);
    assert_eq!(state.active_thread().peek().as_deref(), Some("thread-b"));
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_late_answer_for_an_older_query_is_never_shown() {
    let fixture = Fixture::start(false).await;
    let (mut dom, _props, _state) = with_a_conversation(&fixture).await;
    let (release, receiver) = futures_channel::oneshot::channel();
    let hold = search::Hold { query: "Do the wor".into(), reached: Default::default(), dropped: Default::default(), release: Rc::new(std::cell::RefCell::new(Some(receiver))) };
    search::HOLD.with(|slot| *slot.borrow_mut() = Some(hold.clone()));
    input_control(&mut dom, "Search threads", "Do the wor");
    // The server has answered, and the answer is held on its way to the screen.
    poll_until(&mut dom, "the first answer to be in hand", |_| hold.reached.get()).await;
    assert!(!rendered_text(&dom).contains("You: Do the work 1"), "an answer not yet delivered shows no message hit");
    // The user types on; the newer query is answered and shown.
    input_control(&mut dom, "Search threads", "Do the work");
    wait_for_rendered_text(&mut dom, "You: Do the work 1").await;
    assert!(hold.dropped.get(), "the older request was dropped when the query changed");
    // Letting the old answer go changes nothing.
    drop(release);
    input_control(&mut dom, "Search threads", "Do the work");
    assert!(rendered_text(&dom).contains("You: Do the work 1"));
    assert_eq!(requests(), ["Do the wor", "Do the work"]);
    // The palette finds the same thread by its message and opens it.
    key_on(&mut dom, "Search threads", Key::Escape);
    assert!(!rendered_text(&dom).contains("You: Do the work 1"));
    open_palette(&mut dom).await;
    input_control(&mut dom, PALETTE, "Do the work");
    wait_for_rendered_text(&mut dom, "You: Do the work 1").await;
    assert!(rendered_text(&dom).contains("Threads"));
    click_control(&mut dom, "Do the work 1");
    assert!(!open_palette_text(&dom));
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_connection_that_cannot_read_threads_searches_titles_only() {
    let fixture = Fixture::start(false).await;
    {
        let (mut dom, props) = fixture.client(Gate::default(), &OPERATE);
        seed(&mut dom, &props, &fixture.workspace).await;
    }
    let (mut dom, props) = fixture.client(Gate::default(), &[AuthEnvironmentScope::OrchestrationRead]);
    let state = props.state.borrow().unwrap();
    until(&mut dom, "shell", |_| state.peek().shell.snapshot.as_ref().is_some_and(|snapshot| snapshot.threads.len() == 2)).await;
    input_control(&mut dom, "Search threads", "thread-a");
    assert!(control(&dom, "thread-a").is_some());
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_project_made_from_a_name_opens_a_thread_in_it_and_says_where_it_went() {
    let fixture = Fixture::start(false).await;
    let (mut dom, _props, state) = connect(&fixture, &OPERATE).await;
    click_control(&mut dom, "New project");
    until(&mut dom, "the name step", |dom| control(dom, "Project name").is_some()).await;
    // The server's config names where projects go; the step says so before anything is typed and
    // previews the whole folder once there is a name, as the original does.
    until(&mut dom, "config", |_| state.peek().typed_config.is_some()).await;
    let root = state.peek().typed_config.as_ref().unwrap().new_projects_root.as_ref().expect("the server names its new-projects root").to_string();
    assert!(root.ends_with("projects"), "{root}");
    assert!(rendered_text(&dom).contains(&format!("Goes in {root}")), "{}", rendered_text(&dom));
    input_control(&mut dom, "Project name", "Pinball Stats");
    assert!(rendered_text(&dom).contains(&format!("Creates {root}/pinball-stats")), "{}", rendered_text(&dom));
    key_on(&mut dom, "Project name", Key::Enter);
    // Only the server's answer produces this text.
    wait_for_rendered_text(&mut dom, "Created Pinball Stats").await;
    until(&mut dom, "the project", |_| state.peek().shell.snapshot.as_ref().is_some_and(|snapshot| snapshot.projects.iter().any(|project| project.title.as_str() == "Pinball Stats"))).await;
    // The folder is where the preview said.
    assert!(fixture.directory.path().join("projects/pinball-stats").is_dir());
    // It says where the project went, and warns when the machine could not make the first commit.
    let text = rendered_text(&dom);
    assert!(text.contains("Created Pinball Stats"), "{text}");
    if text.contains("without a first commit") {
        assert!(text.contains("The project is in "), "{text}");
    }
    let project = state.peek().shell.snapshot.as_ref().unwrap().projects.iter().find(|project| project.title.as_str() == "Pinball Stats").unwrap().id.to_string();
    assert_eq!(state.selected_project().peek().as_deref(), Some(project.as_str()), "a new thread is open in it");
    assert!(!open_palette_text(&dom) && control(&dom, "Project name").is_none());
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_first_commit_that_failed_is_reported_beside_where_the_project_is() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props, state) = connect(&fixture, &OPERATE).await;
    let created = crate::project_create::Created { project_id: "p".into(), workspace_root: "/home/me/.t3/projects/pinball-stats".into(), commit_error: Some("Git needs your name and email to commit.".into()) };
    dom.in_scope(ScopeId::APP, || crate::project_create::finish_created(&props.transport, state, "Pinball Stats", &created));
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    let text = rendered_text(&dom);
    assert!(text.contains("Created Pinball Stats without a first commit"), "{text}");
    assert!(text.contains("Git needs your name and email to commit. The project is in /home/me/.t3/projects/pinball-stats."), "{text}");
    click_control(&mut dom, "Dismiss notice");
    assert!(!rendered_text(&dom).contains("without a first commit"));
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_thread_without_a_project_uses_the_no_project_workspace_and_says_why_it_cannot() {
    let fixture = Fixture::start(false).await;
    let (mut dom, _props, state) = connect(&fixture, &OPERATE).await;
    open_palette(&mut dom).await;
    click_control(&mut dom, "New thread without a project");
    until(&mut dom, "the workspace", |_| state.peek().shell.snapshot.as_ref().is_some_and(|snapshot| snapshot.projects.iter().any(|project| project.title.as_str() == "No project"))).await;
    let scratch = state.peek().shell.snapshot.as_ref().unwrap().projects.iter().find(|project| project.title.as_str() == "No project").unwrap().id.to_string();
    until(&mut dom, "selected", |_| state.selected_project().peek().as_deref() == Some(scratch.as_str())).await;
    assert!(state.active_thread().peek().is_none(), "a new thread is being drafted");
    fixture.finish(dom).await;

    // Where the environment offers no such workspace (its home is inside a Git work tree), its
    // config names none, so every way in is withheld, as in the original: nothing is offered and
    // nothing is started.
    let fixture = Fixture::start(false).await;
    super::diff_tests::git(fixture.directory.path(), &["init", "-q"]);
    let (mut dom, _props, state) = connect(&fixture, &OPERATE).await;
    until(&mut dom, "config", |_| state.peek().typed_config.is_some()).await;
    assert!(state.peek().typed_config.as_ref().unwrap().scratch_workspace_root.is_none());
    assert!(press(&mut dom, chord("n", false, true), Dom::default()).await.is_none(), "the shortcut does nothing");
    open_palette(&mut dom).await;
    assert!(control(&dom, "New thread without a project").is_none());
    assert!(!state.peek().shell.snapshot.as_ref().unwrap().projects.iter().any(|project| project.title.as_str() == "No project"));
    fixture.finish(dom).await;
}

fn terminal_harness(props: Harness) -> Element {
    let catalog = crate::terminal_pane::use_catalog();
    catalog.mark_hydrated();
    super::diff_tests::harness(props)
}

#[tokio::test(flavor = "current_thread")]
async fn the_terminal_shortcut_and_the_palette_open_and_close_the_drawer_the_button_does() {
    let fixture = Fixture::start(false).await;
    let (first, props) = fixture.client(Gate::default(), &[AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::SettingsWrite, AuthEnvironmentScope::TerminalRead]);
    drop(first);
    let mut dom = VirtualDom::new_with_props(terminal_harness, props.clone());
    dom.rebuild_in_place();
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "config", |_| state.peek().typed_config.is_some() && has_rule(state, "terminal.toggle", "j")).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    let catalog = dom.in_scope(ScopeId::APP, || try_consume_context::<crate::terminal_pane::Catalog>()).expect("the catalog");
    let key = t3_client::terminal_ui::ScopedPanes::key(state.peek().destination.as_ref().unwrap().as_str(), "thread-a");
    assert!(!catalog.is_open(&key));
    assert_eq!(press(&mut dom, chord("j", false, false), Dom::default()).await.as_deref(), Some("terminal.toggle"));
    poll_until(&mut dom, "the drawer", |dom| control(dom, "Close terminal").is_some() || rendered_text(dom).contains("Hide terminal")).await;
    assert!(catalog.is_open(&key));
    // The palette entry does the same, and the same chord closes it.
    open_palette(&mut dom).await;
    click_control(&mut dom, "Toggle terminal");
    assert!(!catalog.is_open(&key) && !open_palette_text(&dom));
    press(&mut dom, chord("j", false, false), Dom::default()).await;
    assert!(catalog.is_open(&key));
    // Typing into the terminal is the terminal's: the chord that is not meant for the app stays with it.
    assert_eq!(press(&mut dom, typed("j"), Dom { terminal_focus: true, ..Default::default() }).await, None);
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn thread_shortcuts_move_through_the_sidebar_order_and_stop_at_its_ends() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props, state) = connect(&fixture, &OPERATE).await;
    let active = || state.active_thread().peek().clone();
    // The sidebar lists the newest thread first (the active list's order), whatever the names say.
    let order = crate::keys::sidebar_order(state);
    assert_eq!(order.len(), 2, "{order:?}");
    let (first, second) = (order[0].as_str(), order[1].as_str());
    // Whether a chord is claimed depends on which thread is open, through a memo that settles only
    // once the app has rendered the thread. Every step therefore waits for the opened thread to be
    // on screen (the timeline reads its projection) before the next key is pressed.
    macro_rules! opened {
        ($dom:expr, $id:expr) => {
            until($dom, $id, |_| state.peek().thread.projection.as_ref().is_some_and(|projection| projection["thread"]["id"] == $id)).await
        };
    }
    let bracket = |forward: bool, mods: bool| KeyEvent {
        key: if forward { "}" } else { "{" }.into(),
        code: if forward { "BracketRight" } else { "BracketLeft" }.into(),
        meta: keys::platform().is_mac() && mods,
        ctrl: !keys::platform().is_mac() && mods,
        shift: true,
        ..Default::default()
    };
    // With no thread open, next is the first thread; previous would be the last.
    assert_eq!(press(&mut dom, bracket(true, true), Dom::default()).await.as_deref(), Some("thread.next"));
    assert_eq!(active().as_deref(), Some(first));
    opened!(&mut dom, first);
    assert_eq!(press(&mut dom, bracket(true, true), Dom::default()).await.as_deref(), Some("thread.next"));
    assert_eq!(active().as_deref(), Some(second));
    opened!(&mut dom, second);
    // At the last thread there is nothing further, so the chord is not claimed.
    assert_eq!(press(&mut dom, bracket(true, true), Dom::default()).await, None);
    assert_eq!(press(&mut dom, bracket(false, true), Dom::default()).await.as_deref(), Some("thread.previous"));
    assert_eq!(active().as_deref(), Some(first));
    opened!(&mut dom, first);
    // Jumping by number is a desktop shortcut.
    let mut two = chord("2", false, false);
    two.code = "Digit2".into();
    assert_eq!(press(&mut dom, two.clone(), Dom::default()).await.as_deref(), Some("thread.jump.2"));
    assert_eq!(active().as_deref(), Some(second));
    opened!(&mut dom, second);
    two.key = "3".into();
    two.code = "Digit3".into();
    assert_eq!(press(&mut dom, two, Dom::default()).await, None, "there is no third thread");
    let _ = props;
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn send_and_new_thread_sends_the_message_and_opens_a_fresh_composer() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props, state) = connect(&fixture, &OPERATE).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    let mut event = chord("Enter", false, true);
    event.code = "Enter".into();
    // With nothing to send, the chord is claimed but does nothing.
    assert_eq!(press(&mut dom, event.clone(), in_composer()).await.as_deref(), Some("composer.sendAndNewThread"));
    assert_eq!(state.active_thread().peek().as_deref(), Some("thread-a"));
    input_control(&mut dom, "Message", "Run the long job");
    assert_eq!(press(&mut dom, event, in_composer()).await.as_deref(), Some("composer.sendAndNewThread"));
    // The message went to thread-a, and the app is on a new draft.
    until(&mut dom, "the message sent", |_| state.peek().shell.snapshot.as_ref().is_some_and(|snapshot| snapshot.threads.iter().any(|thread| thread.id.as_str() == "thread-a" && thread.latest_run_id.is_some()))).await;
    assert!(state.active_thread().peek().is_none(), "a fresh composer is open");
    fixture.finish(dom).await;
}

/// The palette is the last thing in the page, so its entry is the last control with the title it
/// shares with a button of the drawer.
fn click_palette_entry(dom: &mut VirtualDom, title: &str) {
    let last = all_controls(dom, title).len() - 1;
    crate::runtime::transport_tests::click_nth_control(dom, title, last);
}

const TERMINAL: [AuthEnvironmentScope; 5] = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::SettingsWrite, AuthEnvironmentScope::TerminalRead, AuthEnvironmentScope::TerminalOperate];
const IN_TERMINAL: Dom = Dom { terminal_focus: true, editable_focus: false, composer_focus: false, dialog_open: false };

/// A drawer's terminals: how many, and how many sit in the active group and which way they split.
fn terminals(catalog: &crate::terminal_pane::Catalog, key: &str) -> (bool, usize, usize, Option<t3_client::terminal_ui::Direction>) {
    let pane = catalog.panes.peek().get(key);
    let groups = t3_client::terminal_drawer::displayed_groups(&pane);
    let active = groups.iter().find(|group| group.id == pane.active_terminal_group_id);
    (pane.terminal_open, pane.terminal_ids.len(), active.map_or(0, |group| group.terminal_ids.len()), active.and_then(|group| group.split_direction))
}
async fn terminal_thread(fixture: &Fixture, scopes: &[AuthEnvironmentScope]) -> (VirtualDom, Store<UiModel>, crate::terminal_pane::Catalog, String) {
    let (first, props) = fixture.client(Gate::default(), scopes);
    drop(first);
    let mut dom = VirtualDom::new_with_props(terminal_harness, props.clone());
    dom.rebuild_in_place();
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "config", |_| state.peek().typed_config.is_some() && has_rule(state, "terminal.split", "d")).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    let catalog = dom.in_scope(ScopeId::APP, || try_consume_context::<crate::terminal_pane::Catalog>()).expect("the catalog");
    let key = t3_client::terminal_ui::ScopedPanes::key(state.peek().destination.as_ref().unwrap().as_str(), "thread-a");
    (dom, state, catalog, key)
}

#[tokio::test(flavor = "current_thread")]
async fn the_terminal_shortcuts_the_palette_and_the_drawer_buttons_share_one_way_to_add_split_and_close() {
    use t3_client::terminal_ui::Direction;
    let fixture = Fixture::start(false).await;
    let (mut dom, _state, catalog, key) = terminal_thread(&fixture, &TERMINAL).await;
    assert_eq!(terminals(&catalog, &key).0, false);
    // The same chord means something else outside a terminal: `when` decides.
    assert_eq!(press(&mut dom, chord("d", false, false), Dom::default()).await.as_deref(), Some("diff.toggle"));
    assert_eq!(press(&mut dom, chord("d", false, false), Dom::default()).await.as_deref(), Some("diff.toggle"));
    assert_eq!(terminals(&catalog, &key), (false, 0, 0, None), "outside a terminal the terminal chord does nothing to it");
    // With the drawer closed, a new terminal and a split are offered (and open it); closing is not.
    open_palette(&mut dom).await;
    for offered in ["New terminal", "Split terminal", "Split terminal vertically", "Toggle terminal"] {
        assert!(control(&dom, offered).is_some(), "{offered}");
    }
    assert!(control(&dom, "Close terminal").is_none(), "there is no open drawer to close a terminal in");
    key_on(&mut dom, PALETTE, Key::Escape);

    // A new terminal opens the drawer (which brings its default terminal, as opening it does); a
    // split puts one beside the active one; a vertical one changes the direction.
    assert_eq!(press(&mut dom, chord("n", false, false), IN_TERMINAL).await.as_deref(), Some("terminal.new"));
    let (open, total, in_group, _) = terminals(&catalog, &key);
    assert_eq!((open, in_group), (true, 1));
    assert_eq!(press(&mut dom, chord("d", false, false), IN_TERMINAL).await.as_deref(), Some("terminal.split"));
    assert_eq!(terminals(&catalog, &key), (true, total + 1, 2, None));
    assert_eq!(press(&mut dom, chord("d", true, false), IN_TERMINAL).await.as_deref(), Some("terminal.splitVertical"));
    assert_eq!(terminals(&catalog, &key), (true, total + 2, 3, Some(Direction::Vertical)));
    assert_eq!(press(&mut dom, chord("d", false, false), IN_TERMINAL).await.as_deref(), Some("terminal.split"));
    assert_eq!(terminals(&catalog, &key).2, t3_client::terminal_ui::MAX_PER_GROUP);
    // A group holds only so many: the chord is still the terminal's, and nothing is added.
    assert_eq!(press(&mut dom, chord("d", false, false), IN_TERMINAL).await.as_deref(), Some("terminal.split"));
    assert_eq!(terminals(&catalog, &key).1, total + 3);
    // The drawer's own button is the same code: a new terminal starts a group of its own.
    click_control(&mut dom, "New terminal");
    assert_eq!(terminals(&catalog, &key).1, total + 4);
    // The palette runs it too, and closes first.
    open_palette(&mut dom).await;
    click_palette_entry(&mut dom, "New terminal");
    assert!(!open_palette_text(&dom));
    let all = total + 5;
    assert_eq!(terminals(&catalog, &key).1, all);

    // Closing asks first, however it was asked for; Escape declines.
    let question = "Close terminal?";
    assert_eq!(press(&mut dom, chord("w", false, false), IN_TERMINAL).await.as_deref(), Some("terminal.close"));
    assert!(control(&dom, question).is_some() && rendered_text(&dom).contains("This stops the running process and clears its history."));
    key_on(&mut dom, question, Key::Escape);
    assert!(control(&dom, question).is_none());
    assert_eq!(terminals(&catalog, &key).1, all);
    open_palette(&mut dom).await;
    click_palette_entry(&mut dom, "Close terminal");
    assert!(control(&dom, question).is_some(), "the palette asks the same question");
    click_control(&mut dom, "Cancel close terminal");
    assert!(control(&dom, question).is_none() && terminals(&catalog, &key).1 == all);
    click_control(&mut dom, "Close terminal");
    assert!(control(&dom, question).is_some(), "so does the drawer's button");
    click_control(&mut dom, "Confirm close terminal");
    assert_eq!(terminals(&catalog, &key).1, all - 1);
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn without_terminal_operate_the_terminal_chords_keep_their_native_meaning_and_the_palette_offers_nothing() {
    let fixture = Fixture::start(false).await;
    let (mut dom, _state, catalog, key) = terminal_thread(&fixture, &[AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::SettingsWrite, AuthEnvironmentScope::TerminalRead]).await;
    for event in [chord("n", false, false), chord("d", false, false), chord("d", true, false), chord("w", false, false)] {
        assert_eq!(press(&mut dom, event, IN_TERMINAL).await, None);
    }
    assert_eq!(terminals(&catalog, &key), (false, 0, 0, None));
    open_palette(&mut dom).await;
    assert!(control(&dom, "Toggle terminal").is_some());
    for missing in ["New terminal", "Split terminal", "Split terminal vertically", "Close terminal"] {
        assert!(control(&dom, missing).is_none(), "{missing}");
    }
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn the_theme_editor_toggles_from_its_shortcut_and_the_palette_when_the_theme_library_works() {
    use t3_client::themes::library::Library;
    let fixture = Fixture::start(false).await;
    let (mut dom, ..) = terminal_thread(&fixture, &TERMINAL).await;
    let themes = dom.in_scope(ScopeId::APP, || try_consume_context::<crate::themes::Themes>()).expect("the theme service");
    let (mut library, editor) = (themes.library, themes.editor);
    let shortcut = || KeyEvent { alt: true, ..chord("t", true, false) };
    assert!(editor.peek().is_none());
    // Where the theme library cannot be used, the Create theme button is off and so is the command.
    library.set(Some(Library::Unavailable { reason: "no storage" }));
    assert_eq!(press(&mut dom, shortcut(), IN_TERMINAL).await.as_deref(), Some("themeEditor.toggle"));
    assert!(editor.peek().is_none());
    // Otherwise it has no `when`, so it works from a terminal too, and the same chord closes it.
    library.set(Some(Library::Ready { stored: vec![], themes: vec![] }));
    assert_eq!(press(&mut dom, shortcut(), IN_TERMINAL).await.as_deref(), Some("themeEditor.toggle"));
    assert!(editor.peek().is_some());
    poll_until(&mut dom, "the editor", |dom| control(dom, "Close theme editor").is_some()).await;
    assert_eq!(press(&mut dom, shortcut(), Dom::default()).await.as_deref(), Some("themeEditor.toggle"));
    assert!(editor.peek().is_none());
    poll_until(&mut dom, "the editor to close", |dom| control(dom, "Close theme editor").is_none()).await;
    // The palette lists it with the shortcut, and runs the same command. (The test has no theme
    // storage, so the service may have put its own "unavailable" back by now.)
    open_palette(&mut dom).await;
    assert!(rendered_text(&dom).contains(&label_for("themeEditor.toggle")), "the palette shows the shortcut");
    library.set(Some(Library::Ready { stored: vec![], themes: vec![] }));
    click_control(&mut dom, "Toggle theme editor");
    assert!(!open_palette_text(&dom), "the palette closes first");
    assert!(editor.peek().is_some(), "and the palette's entry opens the editor");
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn the_local_new_thread_shortcut_starts_a_thread_from_any_page_and_not_from_a_terminal() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props, state) = connect(&fixture, &OPERATE).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    assert_eq!(press(&mut dom, chord("n", true, false), IN_TERMINAL).await, None, "`!terminalFocus` keeps it out of a terminal");
    assert_eq!(state.active_thread().peek().as_deref(), Some("thread-a"));
    assert_eq!(press(&mut dom, chord("n", true, false), Dom::default()).await.as_deref(), Some("chat.newLocal"));
    assert!(state.active_thread().peek().is_none() && state.peek().view == View::Chat);
    // From a settings page it returns to the chat, as the sidebar's button does.
    state.view().set(View::Keybindings);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    assert_eq!(press(&mut dom, chord("n", true, false), Dom::default()).await.as_deref(), Some("chat.newLocal"));
    assert_eq!(state.peek().view, View::Chat);
    // Not offered where there is no project to start in.
    fixture.finish(dom).await;
}
