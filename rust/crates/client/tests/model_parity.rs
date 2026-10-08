// Ported behavioral cases from shared/model, web modelSelection/providerModels,
// composerProviderState, and mobile thread-settings-options tests.
#[allow(dead_code)]
#[path = "../src/models.rs"]
mod models;
use models::*;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use t3_contracts::*;
fn decode<T: DeserializeOwned>(value: Value) -> T {
    serde_json::from_value(value).unwrap()
}
fn model(slug: &str, name: &str, caps: Value) -> ServerProviderModel {
    decode(json!({"slug":slug,"name":name,"isCustom":false,"capabilities":caps}))
}
fn caps() -> ModelCapabilities {
    decode(json!({"optionDescriptors":[
        {"type":"select","id":"effort","label":"Effort","currentValue":"medium","options":[{"id":"medium","label":"Medium","isDefault":true},{"id":"high","label":"High"},{"id":"ultrathink","label":"Ultrathink"}],"promptInjectedValues":["ultrathink"]},
        {"type":"boolean","id":"fastMode","label":"Fast","currentValue":true},
        {"type":"select","id":"agent","label":"Agent","currentValue":"plan","options":[{"id":"plan","label":"Plan","isDefault":true},{"id":"build","label":"Build"}]},
        {"type":"select","id":"variant","label":"Variant","options":[{"id":"high","label":"High","isDefault":true}]}
    ]}))
}
fn selections(value: Value) -> Vec<ProviderOptionSelection> {
    decode(value)
}
#[test]
fn exact_identifiers_outrank_names_and_aliases() {
    let mut first = model("gpt-5.4", "5.3", Value::Null);
    first.aliases = Some(Some(vec![TrimmedNonEmptyString::new("5.3").unwrap()]));
    let raw = [
        first,
        model("5.3", "Custom", Value::Null),
        model("gpt-5.3-codex", "Other", Value::Null),
    ];
    let rows: Vec<_> = raw.iter().map(ModelOption::from).collect();
    assert_eq!(
        resolve_selectable_model("codex", " 5.3 ", &rows),
        Some("5.3")
    );
    assert_eq!(
        resolve_selectable_model("codex", "CUSTOM", &rows),
        Some("5.3")
    );
    assert_eq!(
        normalize_model_slug("codex", "5.3").as_deref(),
        Some("gpt-5.3-codex")
    );
    assert_eq!(
        normalize_model_slug("custom-acp", " 5.3 ").as_deref(),
        Some("5.3")
    );
    assert_eq!(resolve_selectable_model("codex", "unreported", &rows), None);
}
#[test]
fn custom_entries_keep_first_owned_slug_and_drop_only_bad_capabilities() {
    let rows = read_custom_models(
        &json!([null, false, " 5.3 ", {"slug":"5.3","name":"Duplicate"}, {"slug":"other","name":" Friendly ","capabilities":{"optionDescriptors":"bad"}}, {"slug":"valid","capabilities":{"optionDescriptors":[]}}, " "]),
    );
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].slug, "5.3");
    assert_eq!(rows[1].name, "Friendly");
    assert_eq!(rows[1].capabilities, None);
    assert!(rows[2].capabilities.is_some());
}
#[test]
fn catalog_ignores_removed_server_custom_rows_and_preserves_metadata_and_order() {
    let mut builtin = model("stock", "Stock", Value::Null);
    builtin.is_default = Some(Some(true));
    builtin.short_name = Some(Some(TrimmedNonEmptyString::new("S").unwrap()));
    let mut stale = model("deleted", "Deleted", Value::Null);
    stale.is_custom = true;
    let raw = [builtin, model("hidden", "Hidden", Value::Null), stale];
    let custom = read_custom_models(&json!(["stock", "custom", "hidden"]));
    let rows = build_catalog(
        "codex",
        &raw,
        &custom,
        &["hidden".into(), "custom".into()],
        &["custom".into()],
        None,
    );
    assert_eq!(
        rows.iter().map(|r| r.slug.as_str()).collect::<Vec<_>>(),
        ["custom", "stock"]
    );
    assert!(rows[1].is_default);
    assert_eq!(rows[1].short_name.as_deref(), Some("S"));
    assert_eq!(
        resolve_catalog_selection("codex", &rows, Some("gone")),
        Some("stock")
    );
}
#[test]
fn custom_model_limit_counts_valid_entries_and_utf16_units() {
    let mut input = vec![json!("builtin"), json!("🦀".repeat(129))];
    input.extend((0..40).map(|i| json!(format!("custom-{i}"))));
    let rows = build_catalog(
        "pi",
        &[model("builtin", "Builtin", Value::Null)],
        &read_custom_models(&Value::Array(input)),
        &[],
        &[],
        None,
    );
    assert_eq!(rows.len(), 33);
    assert_eq!(rows.last().unwrap().slug, "custom-31");
}
#[test]
fn dynamic_missing_selection_is_retained_but_hidden_catalog_aliases_are_authoritative() {
    let mut hidden = model("vendor/model", "Hidden", Value::Null);
    hidden.aliases = Some(Some(vec![TrimmedNonEmptyString::new("alias").unwrap()]));
    assert!(
        build_catalog(
            "opencode",
            &[hidden],
            &[],
            &["vendor/model".into()],
            &[],
            Some("alias")
        )
        .is_empty()
    );
    let missing = build_catalog("opencode", &[], &[], &[], &[], Some(" vendor/old "));
    assert!(missing[0].is_unavailable);
    assert_eq!(
        resolve_catalog_selection("opencode", &missing, Some("vendor/old")),
        Some("vendor/old")
    );
    assert!(build_catalog("codex", &[], &[], &[], &[], Some("old")).is_empty());
    assert!(
        build_catalog(
            "antigravity",
            &[],
            &read_custom_models(&json!(["custom"])),
            &[],
            &[],
            Some(ANTIGRAVITY_DEFAULT_MODEL)
        )
        .is_empty()
    );
}
#[test]
fn implicit_defaults_are_display_only_while_explicit_defaults_overwrite_sessions() {
    let capabilities = caps();
    let descriptors = option_descriptors(&capabilities, None);
    assert!(option_current_value(&descriptors[0], None, None).is_some());
    assert_eq!(explicit_options(&descriptors, None), None);
    let explicit =
        selections(json!([{"id":"effort","value":"medium"},{"id":"unknown","value":true}]));
    let descriptors = option_descriptors(&capabilities, Some(&explicit));
    assert_eq!(
        explicit_options(&descriptors, Some(&explicit)),
        Some(vec![explicit[0].clone()])
    );
    assert_eq!(capabilities, caps()); // Normalization must not mutate discovery descriptors.
}
#[test]
fn invalid_choice_and_prompt_injected_choice_normalize_to_provider_default() {
    for value in ["invalid", "ultrathink"] {
        let explicit = selections(json!([{"id":"effort","value":value}]));
        let descriptors = option_descriptors(&caps(), Some(&explicit));
        let dispatch = explicit_options(&descriptors, Some(&explicit)).unwrap();
        assert_eq!(
            dispatch[0].value,
            ProviderOptionSelectionValue::String(TrimmedNonEmptyString::new("medium").unwrap())
        );
    }
    let explicit = selections(json!([{"id":"fastMode","value":"wrong-type"}]));
    let descriptors = option_descriptors(&caps(), Some(&explicit));
    assert_eq!(
        option_current_value(&descriptors[1], None, None),
        Some(ProviderOptionSelectionValue::Boolean(true))
    );
}
#[test]
fn reported_option_is_display_only_and_owned_by_instance_and_model() {
    let selected: ModelSelection = decode(json!({"instanceId":"custom-cursor","model":"model"}));
    let reported: ModelSelection = decode(
        json!({"instanceId":"custom-cursor","model":"model","options":[{"id":"variant","value":"default"}]}),
    );
    let descriptors = option_descriptors(&caps(), None);
    assert_eq!(
        option_current_value(&descriptors[3], Some(&selected), None),
        None
    );
    assert_eq!(
        option_current_value(&descriptors[3], Some(&selected), Some(&reported)),
        Some(ProviderOptionSelectionValue::String(
            TrimmedNonEmptyString::new("default").unwrap()
        ))
    );
    let foreign: ModelSelection = decode(
        json!({"instanceId":"cursor","model":"model","options":[{"id":"variant","value":"default"}]}),
    );
    assert_eq!(
        option_current_value(&descriptors[3], Some(&selected), Some(&foreign)),
        None
    );
    assert_eq!(
        explicit_options(&descriptors, selected.options.as_deref()),
        None
    );
}
#[test]
fn composer_forces_normal_speed_and_filters_plan_without_mutating_explicit_choices() {
    let raw = [model(
        "model",
        "Model",
        serde_json::to_value(caps()).unwrap(),
    )];
    let options = composer_dispatch_options("cursor", &raw, "model", None, false).unwrap();
    assert_eq!(
        options,
        selections(json!([{"id":"fastMode","value":false}]))
    );
    let explicit =
        selections(json!([{"id":"fastMode","value":true},{"id":"agent","value":"plan"}]));
    let options =
        composer_dispatch_options("cursor", &raw, "model", Some(&explicit), false).unwrap();
    assert_eq!(
        options,
        selections(json!([{"id":"fastMode","value":true},{"id":"agent","value":"build"}]))
    );
    assert_eq!(
        explicit[1].value,
        ProviderOptionSelectionValue::String(TrimmedNonEmptyString::new("plan").unwrap())
    );
}
#[test]
fn missing_opencode_catalog_preserves_explicit_unknown_options_but_drops_plan() {
    let explicit =
        selections(json!([{"id":"reasoning","value":"high"},{"id":"agent","value":"plan"}]));
    assert_eq!(
        composer_dispatch_options("opencode", &[], "vendor/old", Some(&explicit), false),
        Some(vec![explicit[0].clone()])
    );
    assert_eq!(
        composer_dispatch_options("codex", &[], "old", Some(&explicit), false),
        None
    );
}
#[test]
fn selection_comparison_retains_value_types_and_duplicate_multiplicity() {
    let left: ModelSelection = decode(
        json!({"instanceId":"codex","model":"model","options":[{"id":"a","value":false},{"id":"b","value":"high"}]}),
    );
    let mut right = left.clone();
    right.options.as_mut().unwrap().reverse();
    assert!(model_selections_equal(&left, &right));
    right
        .options
        .as_mut()
        .unwrap()
        .push(left.options.as_ref().unwrap()[0].clone());
    assert!(!model_selections_equal(&left, &right));
    let omitted: ModelSelection = decode(json!({"instanceId":"codex","model":"model"}));
    let mut empty = omitted.clone();
    empty.options = Some(vec![]);
    assert!(model_selections_equal(&omitted, &empty));
    assert_eq!(
        model_selection_command(&omitted.instance_id, &left),
        "thread.model-selection.set"
    );
    let other: ModelSelection = decode(json!({"instanceId":"custom-codex","model":"model"}));
    assert_eq!(
        model_selection_command(&omitted.instance_id, &other),
        "provider.switch"
    );
}
#[test]
fn runtime_choices_keep_source_order_and_empty_capability_fallback() {
    assert_eq!(runtime_modes(None), RUNTIME_MODES);
    assert_eq!(runtime_modes(Some(&[])), RUNTIME_MODES);
    let supported = [RuntimeMode::FullAccess, RuntimeMode::ApprovalRequired];
    assert_eq!(
        runtime_modes(Some(&supported)),
        [RuntimeMode::ApprovalRequired, RuntimeMode::FullAccess]
    );
    assert_eq!(
        compatible_runtime_mode(RuntimeMode::Auto, Some(&supported)),
        RuntimeMode::ApprovalRequired
    );
    assert_eq!(runtime_mode_label(RuntimeMode::FullAccess), "Full access");
    assert_eq!(RuntimeMode::default(), RuntimeMode::FullAccess);
}
#[test]
fn mobile_hides_prompt_injection_and_ultracode_without_changing_current_display() {
    let descriptor: ProviderOptionDescriptor = decode(
        json!({"type":"select","id":"effort","label":"Effort","currentValue":"ultrathink","promptInjectedValues":["ultrathink"],"options":[{"id":"high","label":"High"},{"id":"ultrathink","label":"Ultrathink"},{"id":"ultracode","label":"Ultracode"}]}),
    );
    assert_eq!(
        mobile_option_choices(&descriptor)
            .iter()
            .map(|c| c.id.as_str())
            .collect::<Vec<_>>(),
        ["high"]
    );
    assert_eq!(
        option_current_value(&descriptor, None, None),
        Some(ProviderOptionSelectionValue::String(
            TrimmedNonEmptyString::new("ultrathink").unwrap()
        ))
    );
}
