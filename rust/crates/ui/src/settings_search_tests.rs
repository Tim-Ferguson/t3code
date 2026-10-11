//! Finding a setting from the Settings pages and from the command palette, restoring device defaults, the
//! Projects page, and the project-scoped defaults, through the mounted UI and the real native server.
use super::diff_tests::{Fixture, open_thread, rendered_text, seed, until, wait_for_rendered_text};
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::keys_tests::{PALETTE, open_palette};
use crate::runtime::transport_tests::{change_control, click_when_enabled, control, control_attribute, input_control};

const WRITER: [AuthEnvironmentScope; 3] = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::SettingsWrite];

async fn connected(fixture: &Fixture) -> (VirtualDom, super::diff_tests::Harness, Store<UiModel>) {
    let (mut dom, props) = fixture.client(Gate::default(), &WRITER);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the configuration", |_| state.peek().config.get("settings").is_some_and(|settings| settings.is_object())).await;
    (dom, props, state)
}

#[tokio::test(flavor = "current_thread")]
async fn the_palette_finds_a_setting_by_what_it_does_and_opens_its_page() {
    let fixture = Fixture::start(false).await;
    let (mut dom, _props, state) = connected(&fixture).await;
    open_palette(&mut dom).await;
    input_control(&mut dom, PALETTE, "worktree location");
    until(&mut dom, "the setting", |dom| control(dom, "Worktree location").is_some_and(|_| rendered_text(dom).contains("Settings \u{b7} Storage"))).await;
    click_when_enabled(&mut dom, "Worktree location").await;
    until(&mut dom, "the page", |dom| state.peek().view == View::Storage && control(dom, "Worktree location").is_some_and(|(_, value)| value.is_some())).await;
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn the_settings_pages_search_their_own_catalog_and_say_when_nothing_matches() {
    let fixture = Fixture::start(false).await;
    let (mut dom, _props, state) = connected(&fixture).await;
    state.view().set(View::Connections);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    input_control(&mut dom, "Search settings", "send shortcut");
    until(&mut dom, "the result", |dom| control(dom, "Send shortcut").is_some()).await;
    click_when_enabled(&mut dom, "Send shortcut").await;
    until(&mut dom, "the General page", |_| state.peek().view == View::General).await;
    input_control(&mut dom, "Search settings", "zzzzqqqq");
    wait_for_rendered_text(&mut dom, "No settings found.").await;
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn restore_device_defaults_puts_the_preferences_back_and_is_disabled_when_there_is_nothing_to_restore() {
    let fixture = Fixture::start(false).await;
    let (mut dom, _props, state) = connected(&fixture).await;
    state.view().set(View::General);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    let disabled = |dom: &VirtualDom| control_attribute(dom, "Restore device defaults", "disabled").and_then(|(_, value)| value).as_deref() == Some("true");
    until(&mut dom, "the button", |dom| control(dom, "Restore device defaults").is_some()).await;
    assert!(disabled(&dom), "nothing has changed yet");
    change_control(&mut dom, "Time format", "\"24-hour\"");
    until(&mut dom, "the change", |_| state.peek().client_settings.timestamp_format == t3_contracts::TimestampFormat::Hour24Hour).await;
    until(&mut dom, "enabled", |dom| !disabled(dom)).await;
    click_when_enabled(&mut dom, "Restore device defaults").await;
    until(&mut dom, "restored", |_| state.peek().client_settings.timestamp_format == t3_contracts::TimestampFormat::Locale).await;
    until(&mut dom, "disabled again", |dom| disabled(dom)).await;
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn the_projects_page_renames_a_project_and_a_project_overrides_a_default_without_touching_the_environments() {
    let fixture = Fixture::start(false).await;
    let (mut dom, _props, state) = connected(&fixture).await;
    crate::projects_page::open(state, "project");
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    until(&mut dom, "the page", |dom| control(dom, "Project name").is_some()).await;
    input_control(&mut dom, "Project name", "Renamed project");
    click_when_enabled(&mut dom, "Save project name").await;
    until(&mut dom, "renamed", |_| state.peek().shell.snapshot.as_ref().is_some_and(|snapshot| snapshot.projects.iter().any(|project| project.title.as_str() == "Renamed project"))).await;
    // A default chosen here belongs to the project: the environment's value is what it was.
    let environment_mode = state.peek().config["settings"]["defaultRuntimeMode"].clone();
    change_control(&mut dom, "Permissions", "approval-required");
    until(&mut dom, "the override", |_| state.peek().config["settings"]["projectSettingsOverrides"]["project"]["defaultRuntimeMode"] == "approval-required").await;
    assert_eq!(state.peek().config["settings"]["defaultRuntimeMode"], environment_mode);
    // Inherit takes the override away again.
    click_when_enabled(&mut dom, "Use the environment value for defaultRuntimeMode").await;
    until(&mut dom, "inherited", |_| state.peek().config["settings"]["projectSettingsOverrides"]["project"].is_null()).await;
    fixture.finish(dom).await;
}
