//! The rows of the General page that hold one value, through the mounted UI and the real native
//! server: a device preference changes this device's preferences and can be reset, an environment
//! setting is one patch the server stores (and is not offered to a connection without the grant), and
//! the Working shelf folds a busy thread out of Active once its switch is on.
use super::diff_tests::{Fixture, Harness, open_thread, rendered_text, seed, until, wait_for_rendered_text};
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::transport_tests::{change_control, click_control, control, control_attribute, input_control, until_enabled};

const WRITER: [AuthEnvironmentScope; 3] = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::SettingsWrite];
const NO_SETTINGS: [AuthEnvironmentScope; 2] = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate];

async fn with_page(scopes: &[AuthEnvironmentScope], body: impl AsyncFnOnce(&mut VirtualDom, &Harness, Store<UiModel>)) {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.client(Gate::default(), scopes);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the configuration", |_| state.peek().config.get("settings").is_some_and(|settings| settings.is_object())).await;
    state.view().set(View::General);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    body(&mut dom, &props, state).await;
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_device_preference_changes_on_this_device_and_resets_to_the_default() {
    with_page(&NO_SETTINGS, async |dom, _props, state| {
        wait_for_rendered_text(dom, "Send shortcut").await;
        // Nothing to reset while a row holds the default.
        assert!(control(dom, "Reset Send shortcut").is_none());
        change_control(dom, "Send shortcut", "\"mod-enter\"");
        until(dom, "the shortcut", |_| state.client_settings().peek().send_shortcut == t3_contracts::SendShortcut::ModEnter).await;
        until(dom, "the reset", |dom| control(dom, "Reset Send shortcut").is_some()).await;
        click_control(dom, "Reset Send shortcut");
        until(dom, "the default", |_| state.client_settings().peek().send_shortcut == t3_contracts::SendShortcut::Enter).await;
        // A switch, a choice stored as a boolean, and a row of another group.
        change_control(dom, "Archive confirmation", "true");
        until(dom, "the confirmation", |_| state.client_settings().peek().confirm_thread_archive).await;
        change_control(dom, "Default diff file state", "false");
        until(dom, "expanded", |_| !state.client_settings().peek().diff_files_collapsed).await;
        change_control(dom, "Diff layout", "\"split\"");
        until(dom, "split", |_| state.client_settings().peek().diff_layout == t3_contracts::DiffLayout::Split).await;
        // Device preferences do not need the settings grant.
        assert_ne!(control_attribute(dom, "Send shortcut", "disabled").and_then(|(_, value)| value).as_deref(), Some("true"));
    })
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn the_desktop_quit_shortcut_is_chosen_on_the_general_page_and_resets() {
    with_page(&NO_SETTINGS, async |dom, _props, state| {
        wait_for_rendered_text(dom, "Quit shortcut").await;
        assert_eq!(state.client_settings().peek().confirm_quit, t3_contracts::QuitConfirmationMode::Hold, "held is the default");
        assert!(control(dom, "Reset Quit shortcut").is_none());
        change_control(dom, "Quit shortcut", "\"double-click\"");
        until(dom, "double press", |_| state.client_settings().peek().confirm_quit == t3_contracts::QuitConfirmationMode::DoubleClick).await;
        until(dom, "the reset", |dom| control(dom, "Reset Quit shortcut").is_some()).await;
        click_control(dom, "Reset Quit shortcut");
        until(dom, "held again", |_| state.client_settings().peek().confirm_quit == t3_contracts::QuitConfirmationMode::Hold).await;
    })
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn an_environment_setting_is_one_patch_and_needs_the_settings_grant() {
    with_page(&WRITER, async |dom, _props, state| {
        wait_for_rendered_text(dom, "Response streaming").await;
        change_control(dom, "Response streaming", "\"turn\"");
        until(dom, "the server's setting", |_| state.peek().config["settings"]["responseStreamingMode"] == json!("turn")).await;
        // The rows take no change while the first one is being saved.
        until_enabled(dom, "Provider update checks").await;
        change_control(dom, "Provider update checks", "false");
        until(dom, "updates off", |_| state.peek().config["settings"]["enableProviderUpdateChecks"] == json!(false)).await;
        assert_eq!(state.peek().config["settings"]["responseStreamingMode"], json!("turn"), "a patch leaves the other settings");
    })
    .await;
    with_page(&NO_SETTINGS, async |dom, _props, state| {
        wait_for_rendered_text(dom, "Response streaming").await;
        assert_eq!(control_attribute(dom, "Response streaming", "disabled").and_then(|(_, value)| value).as_deref(), Some("true"));
        assert_eq!(control_attribute(dom, "Provider update checks", "disabled").and_then(|(_, value)| value).as_deref(), Some("true"));
        assert_ne!(state.peek().config["settings"]["responseStreamingMode"], json!("turn"));
    })
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_busy_thread_folds_into_the_working_shelf_once_it_is_switched_on_and_comes_back_when_it_is_off() {
    with_page(&WRITER, async |dom, props, state| {
        open_thread(dom, props, state, "thread-a").await;
        input_control(dom, "Message", "Work [hold]");
        dom.in_scope(ScopeId::APP, || send_message(&props.transport, state));
        until(dom, "the turn to run", |dom| control(dom, "Stop generation").is_some()).await;
        // Off, the thread is listed in Active like every other.
        assert!(!rendered_text(dom).contains("Working ("));
        state.view().set(View::General);
        wait_for_rendered_text(dom, "Working section (beta)").await;
        change_control(dom, "Working section (beta)", "true");
        until(dom, "the shelf", |dom| rendered_text(dom).contains("Working (1)")).await;
        assert!(state.client_settings().peek().sidebar_working_shelf_enabled);
        // The shelf folds away; opening it lists the thread.
        click_control(dom, "Working (1)");
        until(dom, "the shelf open", |dom| control(dom, "Working").is_some()).await;
        change_control(dom, "Working section (beta)", "false");
        until(dom, "no shelf", |dom| !rendered_text(dom).contains("Working (1)") && control(dom, "Working").is_none()).await;
    })
    .await;
}

/// The mark of a Dev environment follows the preference: artwork under the brand (and behind the send button), a version
/// pill, or nothing.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn the_environment_mark_follows_the_identification_preference() {
    let marked = |dom: &VirtualDom| control_attribute(dom, "Main sidebar", "class").and_then(|(_, class)| class).is_some_and(|class| class.contains("has-stage-art"));
    let pill = |dom: &VirtualDom| control(dom, "Dev environment").is_some();
    with_page(&NO_SETTINGS, async |dom, _props, state| {
        state.view().set(View::Appearance);
        wait_for_rendered_text(dom, "Environment identification").await;
        until(dom, "the artwork", |dom| marked(dom)).await;
        assert!(control(dom, "Reset Environment identification").is_none(), "artwork is the default");
        change_control(dom, "Environment identification", "\"pill\"");
        until(dom, "the pill", |dom| !marked(dom) && pill(dom)).await;
        assert_eq!(state.client_settings().peek().environment_identification_mode, t3_contracts::EnvironmentIdentificationMode::Pill);
        change_control(dom, "Environment identification", "\"none\"");
        until(dom, "nothing", |dom| !marked(dom) && !pill(dom)).await;
        until(dom, "the reset", |dom| control(dom, "Reset Environment identification").is_some()).await;
        click_control(dom, "Reset Environment identification");
        until(dom, "the artwork again", |dom| marked(dom)).await;
    })
    .await;
}

/// The slider is the contract's range and default, so no value it can send is refused and none the contract accepts is out of reach.
#[test]
fn the_glass_opacity_slider_is_the_contracts_range_and_default() {
    let row = crate::settings_rows::LAYOUT.iter().find(|row| row.key == "glassOpacity").expect("the row is on the Appearance page");
    assert_eq!((row.default, row.range), ("80", Some((40, 100, 5, "%"))));
    let accepts = |value: i64| serde_json::from_value::<t3_contracts::GlassOpacity>(serde_json::json!(value)).is_ok();
    assert!(accepts(40) && accepts(100) && accepts(80));
    assert!(!accepts(35) && !accepts(105));
    assert!(t3_contracts::ClientSettings::default().glass_opacity.0 == 80);
}

/// The conversation's wheel rests the composer once a gesture has gone far enough, and the composer's use lifts it again.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_scrolled_conversation_rests_the_composer_until_it_is_used() {
    use crate::composer_rest::ModelStoreExt as _;
    use crate::runtime::transport_tests::mouse_control;
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.client(Gate::default(), &WRITER);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    let resting = |dom: &VirtualDom| control_attribute(dom, "Message composer", "data-resting").and_then(|(_, value)| value).as_deref() == Some("true");
    let owner = crate::composer_rest::owner(state.destination().peek().as_ref().unwrap().as_str(), "thread-a");
    // The composer says it may be rested, once it has rendered with the setting on.
    until(&mut dom, "the composer's claim", |_| state.composer_rest().eligible_for().peek().as_deref() == Some(owner.as_str())).await;
    assert!(!resting(&dom));
    // Up the conversation, far enough in one gesture.
    crate::timeline_scroll::feed_wheel(state, &owner, 500.0, 3000.0, &[(0.0, -10.0), (16.0, -10.0), (32.0, -10.0)]);
    until(&mut dom, "the resting composer", |dom| resting(dom)).await;
    // Using the composer lifts it.
    mouse_control(&mut dom, "Message composer", "mousedown", false, false, false);
    until(&mut dom, "the composer expanded", |dom| !resting(dom)).await;
    // Down at the end of the conversation, or a short wheel turn, rests nothing.
    crate::timeline_scroll::feed_wheel(state, &owner, 2400.0, 3000.0, &[(1000.0, 50.0), (1016.0, 50.0)]);
    crate::timeline_scroll::feed_wheel(state, &owner, 500.0, 3000.0, &[(2000.0, -10.0)]);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    assert!(!resting(&dom));
    // A draft of several lines keeps the composer open, and so does turning the setting off.
    input_control(&mut dom, "Message", "one\ntwo");
    until(&mut dom, "no claim for a multiline draft", |_| state.composer_rest().eligible_for().peek().is_none()).await;
    crate::timeline_scroll::feed_wheel(state, &owner, 500.0, 3000.0, &[(3000.0, -30.0), (3016.0, -30.0)]);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    assert!(!resting(&dom));
    input_control(&mut dom, "Message", "one");
    until(&mut dom, "the claim again", |_| state.composer_rest().eligible_for().peek().as_deref() == Some(owner.as_str())).await;
    state.view().set(View::General);
    wait_for_rendered_text(&mut dom, "Collapse composer on scroll").await;
    change_control(&mut dom, "Collapse composer on scroll", "false");
    until(&mut dom, "the setting off", |_| !state.client_settings().peek().composer_collapse_on_scroll).await;
    state.view().set(View::Chat);
    until(&mut dom, "no claim", |_| state.composer_rest().eligible_for().peek().is_none()).await;
    crate::timeline_scroll::feed_wheel(state, &owner, 500.0, 3000.0, &[(4000.0, -30.0), (4016.0, -30.0)]);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    assert!(!resting(&dom));
    fixture.finish(dom).await;
}

/// "Composer context" moves the branch control from the header to under the composer.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn composer_context_puts_the_branch_control_under_the_composer() {
    let fixture = Fixture::start(true).await;
    let (mut dom, props) = fixture.client(Gate::default(), &WRITER);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    let in_strip = |dom: &VirtualDom| control_attribute(dom, "Branch controls", "data-composer-context-strip").is_some();
    assert!(!in_strip(&dom));
    state.view().set(View::Appearance);
    wait_for_rendered_text(&mut dom, "Composer context").await;
    change_control(&mut dom, "Composer context", "true");
    until(&mut dom, "the setting", |_| state.client_settings().peek().persist_composer_context_strip).await;
    state.view().set(View::Chat);
    until(&mut dom, "the strip under the composer", |dom| in_strip(dom)).await;
    state.view().set(View::Appearance);
    wait_for_rendered_text(&mut dom, "Composer context").await;
    change_control(&mut dom, "Composer context", "false");
    until(&mut dom, "the setting off", |_| !state.client_settings().peek().persist_composer_context_strip).await;
    state.view().set(View::Chat);
    until(&mut dom, "the strip gone", |dom| !in_strip(dom)).await;
    fixture.finish(dom).await;
}

/// A finished turn that changed enough opens the diff by itself, once "Proactive panels" is on and the person has not chosen otherwise.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_finished_turn_that_changed_enough_opens_the_diff_by_itself() {
    use super::diff_tests::turn;
    let fixture = Fixture::start(true).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    let diff_open = |dom: &VirtualDom| control(dom, "Diff scope").is_some();
    // The first turn changes nothing; it is the baseline the next turn's checkpoint is taken against.
    turn(&mut dom, &props, state, 1).await;
    // Off by default: a turn that changed three files opens nothing.
    fixture.write("a.txt", "a\n");
    fixture.write("b.txt", "b\n");
    fixture.write("c.txt", "c\n");
    turn(&mut dom, &props, state, 2).await;
    wait_for_rendered_text(&mut dom, "3 changed files").await;
    // Let the finished turn be seen with the setting off, so that turning it on later is not what first meets the turn.
    super::diff_tests::pump(&mut dom).await;
    assert!(!diff_open(&dom));
    state.view().set(View::General);
    wait_for_rendered_text(&mut dom, "Proactive panels").await;
    change_control(&mut dom, "Proactive panels", "true");
    until(&mut dom, "the setting", |_| state.client_settings().peek().proactive_panels_enabled).await;
    state.view().set(View::Chat);
    // Turning it on does not reach back to the turn that finished before.
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    assert!(!diff_open(&dom));
    // A small change is not worth a panel.
    fixture.write("a.txt", "a\nb\n");
    turn(&mut dom, &props, state, 3).await;
    wait_for_rendered_text(&mut dom, "1 changed file").await;
    super::diff_tests::pump(&mut dom).await;
    assert!(!diff_open(&dom));
    // Three files are.
    fixture.write("a.txt", "a\nb\nc\n");
    fixture.write("b.txt", "b\nb\n");
    fixture.write("c.txt", "c\nc\n");
    turn(&mut dom, &props, state, 4).await;
    until(&mut dom, "the diff opened by itself", |dom| diff_open(dom)).await;
    fixture.finish(dom).await;
}
