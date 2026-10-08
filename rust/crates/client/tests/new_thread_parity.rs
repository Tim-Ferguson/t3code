use serde_json::{Value, json};
use t3_client::new_thread::*;
use t3_contracts::*;
fn provider(instance: &str, driver: &str, status: &str, models: Value) -> ServerProvider {
    serde_json::from_value(json!({"instanceId":instance,"driver":driver,"enabled":true,"installed":true,"version":null,"status":status,"auth":{"status":"authenticated"},"checkedAt":"2026-10-07T00:00:00Z","models":models,"slashCommands":[],"skills":[]})).unwrap()
}
fn model(slug: &str) -> Value {
    json!({"slug":slug,"name":slug,"isCustom":false,"capabilities":null})
}
fn selection(instance: &str, model: &str) -> ModelSelection {
    serde_json::from_value(json!({"instanceId":instance,"model":model})).unwrap()
}
fn config(providers: Vec<ServerProvider>) -> ServerConfig {
    serde_json::from_value(json!({"environment":{"environmentId":"environment","label":"Fixture","platform":{"os":"linux","arch":"x64"},"serverVersion":"rust-test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false}},"auth":{"policy":"loopback-browser","bootstrapMethods":["one-time-token"],"sessionMethods":["browser-session-cookie","bearer-access-token"],"sessionCookieName":"fixture","serverUpdateScope":"environment:maintain"},"cwd":"/isolated","keybindingsConfigPath":"/isolated/keybindings.json","keybindings":default_resolved_keybindings(),"issues":[],"providers":providers,"availableEditors":[],"observability":{"logsDirectoryPath":"/isolated/logs","localTracingEnabled":false,"otlpTracesEnabled":false,"otlpMetricsEnabled":false},"settings":ServerSettings::default()})).unwrap()
}
fn project() -> ProjectShell {
    serde_json::from_value(json!({"id":"project","title":"Project","workspaceRoot":"/isolated","defaultModelSelection":null,"scripts":[],"createdAt":"2026-10-07T00:00:00Z","updatedAt":"2026-10-07T00:00:00Z"})).unwrap()
}
#[test]
fn requested_error_keeps_unknown_model_and_all_explicit_values_byte_for_byte() {
    let providers = [
        provider("cursor", "cursor", "ready", json!([model("auto")])),
        provider("codex", "codex", "error", json!([model("reported")])),
    ];
    let selected:ModelSelection=serde_json::from_value(json!({"instanceId":"codex","model":"provider-owned-unknown","options":[{"id":"unknown-option","value":false}]})).unwrap();
    assert_eq!(
        resolve_default_provider_model_selection(&providers, Some(&selected)),
        Some(selected.clone())
    );
    let config = config(providers.to_vec());
    assert!(
        available_models(
            &config,
            &ClientSettings::default(),
            Surface::Web,
            Some(&selected)
        )
        .iter()
        .all(|row| row.instance_id.as_str() != "codex")
    );
    let defaults = ProjectDefaults {
        model_selection: Some(selected.clone()),
        runtime_mode: RuntimeMode::FullAccess,
        environment_mode: None,
    };
    assert_eq!(
        resolve_new_thread_selection(
            &config,
            &ClientSettings::default(),
            Surface::Web,
            None,
            &defaults,
            None
        ),
        Some(selected)
    );
}
#[test]
fn fallback_prefers_ready_then_warning_never_invents_error_profile() {
    let error = provider("codex", "codex", "error", json!([model("error-model")]));
    let warning = provider(
        "cursor",
        "cursor",
        "warning",
        json!([model("warning-model")]),
    );
    let ready = provider("pi", "pi", "ready", json!([model("ready-model")]));
    assert_eq!(
        resolve_default_provider_model_selection(&[error.clone(), warning.clone(), ready], None)
            .unwrap()
            .model
            .as_str(),
        "ready-model"
    );
    assert_eq!(
        resolve_default_provider_model_selection(&[error.clone(), warning], None)
            .unwrap()
            .model
            .as_str(),
        "warning-model"
    );
    assert_eq!(
        resolve_default_provider_model_selection(&[error], None),
        None
    );
    assert_eq!(resolve_default_provider_model_selection(&[], None), None);
}
#[test]
fn deleted_disabled_and_unavailable_instance_fallback_resets_only_instance_model_options() {
    for reason in ["missing", "disabled", "unavailable"] {
        let mut removed = provider(
            "custom-codex",
            "codex",
            "ready",
            json!([model("removed-model")]),
        );
        if reason == "disabled" {
            removed.enabled = false;
        }
        if reason == "unavailable" {
            removed.availability = Some(Some(ServerProviderAvailability::Unavailable));
        }
        let mut fallback = provider(
            "custom-cursor",
            "cursor",
            "ready",
            json!([model("first"), model("default")]),
        );
        fallback.models[1].is_default = Some(Some(true));
        let providers = if reason == "missing" {
            vec![fallback]
        } else {
            vec![removed, fallback]
        };
        let stored:ModelSelection=serde_json::from_value(json!({"instanceId":"custom-codex","model":"removed-model","options":[{"id":"fastMode","value":true}]})).unwrap();
        assert_eq!(
            resolve_default_provider_model_selection(&providers, Some(&stored)),
            Some(selection("custom-cursor", "default"))
        );
    }
}
#[test]
fn default_model_prefers_builtin_default_not_custom_or_first_snapshot_row() {
    let mut p = provider(
        "codex",
        "codex",
        "ready",
        json!([
            model("custom-default"),
            model("builtin"),
            model("declared-default")
        ]),
    );
    p.models[0].is_custom = true;
    p.models[0].is_default = Some(Some(true));
    p.models[2].is_default = Some(Some(true));
    assert_eq!(
        resolve_default_provider_model_selection(&[p.clone()], None),
        Some(selection("codex", "declared-default"))
    );
    p.models.clear();
    assert_eq!(
        resolve_default_provider_model_selection(&[p], None),
        Some(selection("codex", "gpt-6-astra"))
    );
}
#[test]
fn instance_custom_array_including_empty_owns_legacy_fallback_and_other_instances_do_not_borrow() {
    let stock = provider("codex", "codex", "ready", json!([]));
    let custom = provider("custom-codex", "codex", "ready", json!([]));
    let mut settings = ServerSettings::default();
    settings.providers.codex.custom_models = vec![CustomModelSetting::String("legacy".into())];
    assert_eq!(instance_custom_models(&settings, &stock)[0].slug, "legacy");
    assert!(instance_custom_models(&settings, &custom).is_empty());
    settings.provider_instances.insert(
        stock.instance_id.clone(),
        serde_json::from_value(json!({"driver":"codex","config":{"customModels":[]}})).unwrap(),
    );
    assert!(instance_custom_models(&settings, &stock).is_empty());
    settings.provider_instances.insert(
        custom.instance_id.clone(),
        serde_json::from_value(json!({"driver":"codex","config":{"customModels":["own"]}}))
            .unwrap(),
    );
    assert_eq!(instance_custom_models(&settings, &custom)[0].slug, "own");
}
#[test]
fn folded_projects_do_not_resurrect_legacy_defaults_and_nullable_override_is_real_reset() {
    let mut settings = ServerSettings::default();
    settings.default_model_selection = Some(selection("codex", "environment"));
    let mut project = project();
    project.default_model_selection = Some(selection("codex", "legacy"));
    project
        .extra
        .insert("defaultThreadEnvMode".into(), json!("worktree"));
    assert_eq!(
        project_defaults(&settings, Some(&project)).model_selection,
        project.default_model_selection
    );
    assert_eq!(
        project_defaults(&settings, Some(&project)).environment_mode,
        Some(ThreadEnvMode::Worktree)
    );
    settings.project_settings_overrides.insert(
        project.id.clone(),
        serde_json::from_value(json!({"defaultModelSelection":null,"defaultRuntimeMode":"auto"}))
            .unwrap(),
    );
    assert_eq!(
        project_defaults(&settings, Some(&project)).model_selection,
        None
    );
    assert_eq!(
        project_defaults(&settings, Some(&project)).runtime_mode,
        RuntimeMode::Auto
    );
    settings.project_settings_overrides.clear();
    settings.project_settings_folded = true;
    assert_eq!(
        project_defaults(&settings, Some(&project)).model_selection,
        settings.default_model_selection
    );
    assert_eq!(
        project_defaults(&settings, Some(&project)).environment_mode,
        settings.default_thread_env_mode
    );
}
#[test]
fn disabled_project_model_override_falls_back_to_environment_value() {
    let mut settings = ServerSettings::default();
    let project = project();
    settings.default_model_selection = Some(selection("codex", "environment"));
    settings.provider_instances.insert(
        ProviderInstanceId::new("custom-codex").unwrap(),
        serde_json::from_value(json!({"driver":"codex","enabled":true,"config":{"enabled":false}}))
            .unwrap(),
    );
    settings.project_settings_overrides.insert(
        project.id.clone(),
        serde_json::from_value(
            json!({"defaultModelSelection":{"instanceId":"custom-codex","model":"disabled"}}),
        )
        .unwrap(),
    );
    assert_eq!(
        project_defaults(&settings, Some(&project)).model_selection,
        settings.default_model_selection
    );
}
#[test]
fn mobile_implicit_legacy_is_rejected_explicit_legacy_and_known_antigravity_are_kept() {
    let mut p = provider(
        "codex",
        "codex",
        "ready",
        json!([model("legacy"), model("current")]),
    );
    p.models[0].is_legacy = Some(Some(true));
    p.models[1].is_default = Some(Some(true));
    let config = config(vec![p]);
    let client = ClientSettings::default();
    let legacy = selection("codex", "legacy");
    let defaults = ProjectDefaults {
        model_selection: Some(legacy.clone()),
        runtime_mode: RuntimeMode::FullAccess,
        environment_mode: None,
    };
    assert_eq!(
        resolve_new_thread_selection(&config, &client, Surface::Mobile, None, &defaults, None),
        Some(selection("codex", "current"))
    );
    assert_eq!(
        resolve_new_thread_selection(
            &config,
            &client,
            Surface::Mobile,
            Some(&legacy),
            &defaults,
            None
        ),
        Some(legacy)
    );
    let mut config = config;
    config.settings.provider_instances.insert(
        ProviderInstanceId::new("custom-ag").unwrap(),
        serde_json::from_value(json!({"driver":"antigravity","enabled":false})).unwrap(),
    );
    let preserved = selection("custom-ag", "missing");
    assert_eq!(
        resolve_new_thread_selection(
            &config,
            &client,
            Surface::Mobile,
            Some(&preserved),
            &defaults,
            None
        ),
        Some(preserved)
    );
}
#[test]
fn late_catalog_first_run_uses_declared_model_and_supported_runtime_without_manual_pick() {
    let mut p = provider(
        "codex",
        "codex",
        "ready",
        json!([model("first"), model("default")]),
    );
    p.models[1].is_default = Some(Some(true));
    p.supported_runtime_modes = Some(Some(ForwardCompatibleArray(vec![
        RuntimeMode::ApprovalRequired,
    ])));
    let config = config(vec![p]);
    let client = ClientSettings::default();
    let defaults = project_defaults(&config.settings, None);
    let selected =
        resolve_new_thread_selection(&config, &client, Surface::Web, None, &defaults, None)
            .unwrap();
    assert_eq!(selected, selection("codex", "default"));
    assert_eq!(
        launch_selection(&config, &client, &selected, defaults.runtime_mode)
            .unwrap()
            .1,
        RuntimeMode::ApprovalRequired
    );
}
