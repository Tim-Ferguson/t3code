use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::Read;
use t3_contracts::*;
fn roundtrip<T: serde::de::DeserializeOwned + Serialize>(input: Value) -> Result<Value, String> {
    serde_json::from_value::<T>(input)
        .map_err(|e| e.to_string())
        .and_then(|v| serde_json::to_value(v).map_err(|e| e.to_string()))
}
fn decode(schema: &str, input: Value) -> Result<Value, String> {
    match schema {
        // BEGIN SOURCE CODEC DISPATCH
        "AppearanceContrast" => roundtrip::<AppearanceContrast>(input),
        "BrowserLinkTarget" => roundtrip::<BrowserLinkTarget>(input),
        "BrowserProfile" => roundtrip::<BrowserProfile>(input),
        "BrowserProfileKind" => roundtrip::<BrowserProfileKind>(input),
        "BrowserProfileName" => roundtrip::<BrowserProfileName>(input),
        "BrowserRecordingFrameRate" => roundtrip::<BrowserRecordingFrameRate>(input),
        "ChatWidth" => roundtrip::<ChatWidth>(input),
        "ClientSettingsPatch" => roundtrip::<ClientSettingsPatch>(input),
        "ClientSettingsSchema" => roundtrip::<ClientSettingsSchema>(input),
        "CodeFontSize" => roundtrip::<CodeFontSize>(input),
        "DiffColorScheme" => roundtrip::<DiffColorScheme>(input),
        "DiffLayout" => roundtrip::<DiffLayout>(input),
        "EnvironmentIdentificationMode" => roundtrip::<EnvironmentIdentificationMode>(input),
        "FontFamilyPreference" => roundtrip::<FontFamilyPreference>(input),
        "GlassOpacity" => roundtrip::<GlassOpacity>(input),
        "InterfaceFontSize" => roundtrip::<InterfaceFontSize>(input),
        "LoadBalancingWeights" => roundtrip::<LoadBalancingWeights>(input),
        "NotificationMode" => roundtrip::<NotificationMode>(input),
        "PanelAnimationDurationMs" => roundtrip::<PanelAnimationDurationMs>(input),
        "PreviewAppearancePreference" => roundtrip::<PreviewAppearancePreference>(input),
        "PreviewViewportSetting" => roundtrip::<PreviewViewportSetting>(input),
        "PreviewZoomFactor" => roundtrip::<PreviewZoomFactor>(input),
        "PromptFontSize" => roundtrip::<PromptFontSize>(input),
        "QuitConfirmationMode" => roundtrip::<QuitConfirmationMode>(input),
        "SidebarProjectGroupingMode" => roundtrip::<SidebarProjectGroupingMode>(input),
        "SidebarProjectSortOrder" => roundtrip::<SidebarProjectSortOrder>(input),
        "SidebarThreadPreviewCount" => roundtrip::<SidebarThreadPreviewCount>(input),
        "SidebarThreadSortOrder" => roundtrip::<SidebarThreadSortOrder>(input),
        "SnapShotKeyChord" => roundtrip::<SnapShotKeyChord>(input),
        "SnapShotModifier" => roundtrip::<SnapShotModifier>(input),
        "SnapShotShortcut" => roundtrip::<SnapShotShortcut>(input),
        "SnapShotSound" => roundtrip::<SnapShotSound>(input),
        "TerminalFontSize" => roundtrip::<TerminalFontSize>(input),
        "TimestampFormat" => roundtrip::<TimestampFormat>(input),
        // END SOURCE CODEC DISPATCH
        other => panic!("Unmapped source codec {other}"),
    }
}
#[derive(Deserialize)]
struct Case {
    schema: String,
    label: String,
    input: Value,
    valid: bool,
    output: Option<Value>,
}
#[test]
fn client_settings_match_source_json_codec() {
    let bytes = include_bytes!("fixtures/client-settings.jsonl.gz");
    let mut decoder = flate2::read::GzDecoder::new(bytes.as_slice());
    let mut fixture = String::new();
    decoder.read_to_string(&mut fixture).unwrap();
    let mut failures = vec![];
    let mut count = 0;
    for line in fixture.lines() {
        count += 1;
        let case: Case = serde_json::from_str(line).unwrap();
        let result = decode(&case.schema, case.input.clone());
        if result.is_ok() != case.valid
            || case.valid && result.as_ref().ok() != case.output.as_ref()
        {
            failures.push(format!(
                "{}: {} input={} expected={:?} actual={:?}",
                case.schema, case.label, case.input, case.output, result
            ));
        }
    }
    assert!(
        count > 1000,
        "fixture must cover complete and partial settings boundaries"
    );
    assert!(
        failures.is_empty(),
        "{} divergences\n{}",
        failures.len(),
        failures.into_iter().take(8).collect::<Vec<_>>().join("\n")
    );
}
#[test]
fn builtin_profiles_cannot_be_shadowed_and_duplicates_use_first() {
    let profiles: Vec<BrowserProfile> = serde_json::from_value(json!([
       {"id":"default","name":"Shadow","kind":"persistent"},
       {"id":"work","name":"Work","kind":"incognito"},
       {"id":"work","name":"Duplicate","kind":"persistent"}
    ]))
    .unwrap();
    let resolved = resolve_browser_profiles(&profiles);
    assert_eq!(resolved.len(), 3);
    assert_eq!(resolved[0].name.0.as_str(), "Default");
    assert_eq!(resolved[2].name.0.as_str(), "Work");
    assert_eq!(resolved[2].kind, BrowserProfileKind::Persistent);
}
#[test]
fn settings_preserve_local_preferences_and_patch_nullability() {
    let settings:ClientSettings=serde_json::from_value(json!({"confirmQuit":false,"providerModelPreferences":{"codex":{"hiddenModels":["gpt-5"],"modelOrder":["gpt-6"]}},"favorites":[{"provider":"codex","model":"gpt-6"}]})).unwrap();
    assert_eq!(settings.confirm_quit, QuitConfirmationMode::Direct);
    assert!(!settings.plan_mode_enabled);
    assert_eq!(settings.favorites.len(), 1);
    assert!(serde_json::from_value::<ClientSettingsPatch>(json!({"confirmQuit":false})).is_err());
    assert!(serde_json::from_value::<ClientSettingsPatch>(json!({"favorites":null})).is_err());
    let patch: ClientSettingsPatch =
        serde_json::from_value(json!({"onboardingCompletedAt":null})).unwrap();
    assert_eq!(
        serde_json::to_value(patch).unwrap(),
        json!({"onboardingCompletedAt":null})
    );
}
