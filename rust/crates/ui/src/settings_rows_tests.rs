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
