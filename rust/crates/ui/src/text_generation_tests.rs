//! The text generation settings page and the Git fetch interval row through the mounted UI and
//! the real native server: each setting is one atomic patch the server accepts, a project's
//! override stores and clears separately from the environment's, only the right instances are
//! offered, and a connection without the settings grant changes nothing.
use super::diff_tests::{Fixture, Harness, error, rendered_text, seed, until, wait_for_rendered_text};
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::transport_tests::{change_control, click_control, control, control_attribute};
use t3_client::text_settings as logic;

const WRITER: [AuthEnvironmentScope; 3] = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::SettingsWrite];
const NO_SETTINGS: [AuthEnvironmentScope; 2] = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate];

async fn with_page(view: View, scopes: &[AuthEnvironmentScope], body: impl AsyncFnOnce(&mut VirtualDom, &Harness, Store<UiModel>)) {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.client(Gate::default(), scopes);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the configuration", |_| state.peek().config.get("settings").is_some_and(|settings| settings.is_object())).await;
    state.view().set(view);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    body(&mut dom, &props, state).await;
    fixture.finish(dom).await;
}
fn setting(state: Store<UiModel>, path: &[&str]) -> Value {
    path.iter().fold(state.peek().config["settings"].clone(), |value, key| value[*key].clone())
}
fn disabled(dom: &VirtualDom, label: &str) -> bool {
    control_attribute(dom, label, "disabled").and_then(|(_, value)| value).as_deref() == Some("true")
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn the_environments_text_generation_settings_are_saved_one_patch_at_a_time_and_only_fit_instances_are_offered() {
    with_page(View::TextGeneration, &WRITER, async |dom, _props, state| {
        wait_for_rendered_text(dom, "Worktree branch naming").await;
        let choices = logic::choices(&state.peek().config["providers"]);
        assert!(!choices.is_empty() && choices.iter().all(|choice| choice.instance_id == "codex"), "only the enabled codex instance: {choices:?}");
        // The text generation model.
        let chosen = choices.last().unwrap().clone();
        change_control(dom, "Text generation model", &chosen.key());
        until(dom, "the model", |_| setting(state, &["textGenerationModelSelection"])["model"] == chosen.model.as_str()).await;
        // Branch naming: the mode, then what the mode asks for.
        assert!(control(dom, "Branch prefix").is_some() && control(dom, "Branch naming instructions").is_none());
        change_control(dom, "Branch prefix", " t3/ ");
        until(dom, "the prefix", |_| setting(state, &["branchNamePrefix"]) == json!("t3/")).await;
        change_control(dom, "Worktree branch naming", "custom");
        until(dom, "the mode", |_| setting(state, &["branchNamingMode"]) == json!("custom")).await;
        change_control(dom, "Branch naming instructions", "Use julius/ and the issue id.");
        until(dom, "the instructions", |_| setting(state, &["branchNameInstructions"]) == json!("Use julius/ and the issue id.")).await;
        change_control(dom, "Worktree branch naming", "semantic");
        until(dom, "semantic", |_| setting(state, &["branchNamingMode"]) == json!("semantic")).await;
        wait_for_rendered_text(dom, "The model chooses a prefix that describes the work").await;
        // The writing style, its templates switch, and a writer model of its own.
        change_control(dom, "Source control writing style", "custom");
        until(dom, "the style", |_| setting(state, &["sourceControlWritingStyle", "mode"]) == json!("custom")).await;
        change_control(dom, "Custom source control writing instructions", "Keep titles short.");
        until(dom, "the style instructions", |_| setting(state, &["sourceControlWritingStyle", "customInstructions"]) == json!("Keep titles short.")).await;
        assert_eq!(setting(state, &["sourceControlWritingStyle", "mode"]), json!("custom"), "a field changed leaves the others");
        change_control(dom, "Follow change request templates", "false");
        until(dom, "templates off", |_| setting(state, &["sourceControlWritingStyle", "followChangeRequestTemplates"]) == json!(false)).await;
        assert!(control(dom, "Source control writer model").is_none());
        change_control(dom, "Use a separate source control writer model", "true");
        until(dom, "a writer", |_| setting(state, &["sourceControlWriterModelSelection"]).is_object()).await;
        until(dom, "the writer select", |dom| control(dom, "Source control writer model").is_some()).await;
        change_control(dom, "Use a separate source control writer model", "false");
        until(dom, "no writer", |_| setting(state, &["sourceControlWriterModelSelection"]).is_null()).await;
        assert_eq!(error(state), None);
    })
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_project_overrides_a_setting_on_its_own_and_inherits_it_again() {
    with_page(View::TextGeneration, &WRITER, async |dom, _props, state| {
        wait_for_rendered_text(dom, "Worktree branch naming").await;
        change_control(dom, "Text generation scope", "project");
        wait_for_rendered_text(dom, "Inherited from the environment.").await;
        change_control(dom, "Worktree branch naming", "custom");
        until(dom, "the override", |_| setting(state, &["projectSettingsOverrides", "project", "branchNamingMode"]) == json!("custom")).await;
        assert_eq!(setting(state, &["branchNamingMode"]), json!("static"), "the environment is untouched");
        wait_for_rendered_text(dom, "Overridden for this project.").await;
        click_control(dom, "Use the environment value for branchNamingMode");
        until(dom, "the override gone", |_| setting(state, &["projectSettingsOverrides", "project"]).is_null()).await;
        assert_eq!(error(state), None);
    })
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn without_the_settings_grant_the_page_reads_and_changes_nothing() {
    with_page(View::TextGeneration, &NO_SETTINGS, async |dom, _props, state| {
        wait_for_rendered_text(dom, "This connection can view these settings but cannot change them.").await;
        for label in ["Text generation model", "Source control writing style", "Worktree branch naming"] {
            assert!(disabled(dom, label), "{label}");
        }
        assert_eq!(setting(state, &["branchNamingMode"]), json!("static"));
    })
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn the_git_fetch_interval_is_the_policys_and_resets_to_the_presets() {
    with_page(View::SourceControl, &WRITER, async |dom, _props, state| {
        wait_for_rendered_text(dom, "Git fetch interval").await;
        let value = |dom: &VirtualDom| control_attribute(dom, "Automatic Git fetch interval in seconds", "value").and_then(|(_, value)| value);
        assert_eq!(value(dom).as_deref(), Some("30"), "the balanced preset");
        assert!(control(dom, "Reset fetch interval").is_none());
        click_control(dom, "Increase fetch interval");
        until(dom, "35 seconds", |_| setting(state, &["backgroundActivity", "overrides", "automaticGitFetchInterval"]) == json!(35_000)).await;
        assert_eq!(setting(state, &["backgroundActivity", "profile"]), json!("custom"));
        until(dom, "the field", |dom| value(dom).as_deref() == Some("35")).await;
        change_control(dom, "Automatic Git fetch interval in seconds", "0");
        until(dom, "off", |_| setting(state, &["backgroundActivity", "overrides", "automaticGitFetchInterval"]) == json!(0)).await;
        click_control(dom, "Reset fetch interval");
        until(dom, "the preset again", |_| setting(state, &["backgroundActivity", "overrides", "automaticGitFetchInterval"]).is_null()).await;
        until(dom, "30 again", |dom| value(dom).as_deref() == Some("30")).await;
        assert!(rendered_text(dom).contains("Set to 0 to avoid automatic Git prompts."));
        assert_eq!(error(state), None);
    })
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn without_the_settings_grant_the_fetch_interval_cannot_be_changed() {
    with_page(View::SourceControl, &NO_SETTINGS, async |dom, _props, _state| {
        wait_for_rendered_text(dom, "Git fetch interval").await;
        assert!(disabled(dom, "Increase fetch interval") && disabled(dom, "Automatic Git fetch interval in seconds"));
    })
    .await;
}
