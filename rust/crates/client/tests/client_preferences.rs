use serde_json::{Value, json};
use t3_client::client_preferences::Preferences;
#[test]
fn lossless_patch_preserves_unrelated_unknown_keys_and_legacy_values() {
    let raw = json!({"fontSizeCode":14,"confirmQuit":true,"providerModelPreferences":{"codex":{"hiddenModels":["a"],"modelOrder":["b"]}},"futurePreferences":{"nested":[null,"untouched",42]}});
    let mut state = Preferences::default();
    let generation = state.begin_hydration();
    state
        .hydrate(generation, Ok(Some(&raw.to_string())))
        .unwrap();
    assert!(state.write_receipt().is_none());
    state
        .patch(json!({"fontFamilyTerminal":"Menlo","fontSizeTerminal":16}))
        .unwrap();
    let receipt = state.write_receipt().unwrap();
    let actual: Value = serde_json::from_str(&receipt.bytes).unwrap();
    for key in [
        "confirmQuit",
        "providerModelPreferences",
        "futurePreferences",
        "fontSizeCode",
    ] {
        assert_eq!(actual[key], raw[key]);
    }
    assert_eq!(actual["fontFamilyTerminal"], "Menlo");
    assert_eq!(actual["fontSizeTerminal"], 16);
    state.acknowledge(&receipt);
    assert!(state.write_receipt().is_none());
}
#[test]
fn early_patches_wait_for_hydration_then_apply_in_order_over_saved_preferences() {
    let mut state = Preferences::default();
    let generation = state.begin_hydration();
    state.patch(json!({"fontSizeCode":14})).unwrap();
    state
        .patch(json!({"fontSizeCode":16,"fontFamilyCode":"Menlo"}))
        .unwrap();
    assert_eq!(state.snapshot().font_size_code.0, 13);
    assert!(state.write_receipt().is_none());
    state
        .hydrate(
            generation,
            Ok(Some(
                r#"{"fontSizeCode":12,"fontSizeTerminal":18,"future":"saved"}"#,
            )),
        )
        .unwrap();
    assert_eq!(state.snapshot().font_size_code.0, 16);
    assert_eq!(state.snapshot().font_size_terminal.0, 18);
    let raw: Value = serde_json::from_str(&state.write_receipt().unwrap().bytes).unwrap();
    assert_eq!(raw["future"], "saved");
}
#[test]
fn failed_reads_and_malformed_documents_never_become_writable_then_retry_retains_edits() {
    for raw in [
        "null",
        "[]",
        "not json",
        r#"{"fontSizeCode":999,"future":true}"#,
    ] {
        let mut state = Preferences::default();
        state.patch(json!({"fontSizeCode":14})).unwrap();
        let generation = state.begin_hydration();
        assert!(state.hydrate(generation, Ok(Some(raw))).is_err());
        assert!(!state.hydrated());
        assert!(state.write_receipt().is_none());
        let generation = state.begin_hydration();
        state
            .hydrate(generation, Ok(Some(r#"{"future":true}"#)))
            .unwrap();
        assert_eq!(state.snapshot().font_size_code.0, 14);
        assert_eq!(
            serde_json::from_str::<Value>(&state.write_receipt().unwrap().bytes).unwrap()["future"],
            true
        );
        assert!(state.read_error.is_none());
    }
}
#[test]
fn old_hydration_and_delayed_write_receipts_cannot_overwrite_new_edits() {
    let mut state = Preferences::default();
    let old = state.begin_hydration();
    let new = state.begin_hydration();
    assert!(
        !state
            .hydrate(old, Ok(Some(r#"{"fontSizeCode":10}"#)))
            .unwrap()
    );
    state.hydrate(new, Ok(None)).unwrap();
    state.patch(json!({"fontSizeCode":14})).unwrap();
    let first = state.write_receipt().unwrap();
    state.patch(json!({"fontSizeCode":18})).unwrap();
    state.acknowledge(&first);
    let last = state.write_receipt().unwrap();
    assert!(last.revision > first.revision);
    assert_eq!(
        serde_json::from_str::<Value>(&last.bytes).unwrap()["fontSizeCode"],
        18
    );
    state.failed_write("disk unavailable".into());
    assert_eq!(state.write_receipt(), Some(last.clone()));
    state.acknowledge(&last);
    assert!(state.write_error.is_none());
    assert!(state.write_receipt().is_none());
}
#[test]
fn malformed_patch_is_atomic_and_does_not_repair_invalid_persistence() {
    let mut state = Preferences::default();
    let generation = state.begin_hydration();
    state
        .hydrate(generation, Ok(Some(r#"{"future":1}"#)))
        .unwrap();
    assert!(
        state
            .patch(json!({"fontFamilyCode":"Menlo","fontSizeCode":999}))
            .is_err()
    );
    assert_eq!(state.snapshot().font_family_code.0, "");
    assert!(state.write_receipt().is_none());
}
#[test]
fn retry_read_preserves_unsaved_edits_and_blocks_writes_after_read_failure() {
    let mut state = Preferences::default();
    let generation = state.begin_hydration();
    state
        .hydrate(generation, Ok(Some(r#"{"future":"original"}"#)))
        .unwrap();
    state.patch(json!({"fontSizeCode":16})).unwrap();
    state.failed_write("quota".into());
    let generation = state.begin_hydration();
    assert!(
        state
            .hydrate(generation, Err("read blocked".into()))
            .is_err()
    );
    assert!(!state.hydrated());
    assert!(state.write_receipt().is_none());
    let generation = state.begin_hydration();
    state
        .hydrate(
            generation,
            Ok(Some(r#"{"fontSizeCode":10,"future":"other-tab-new-key"}"#)),
        )
        .unwrap();
    assert_eq!(state.snapshot().font_size_code.0, 16);
    assert_eq!(
        serde_json::from_str::<Value>(&state.write_receipt().unwrap().bytes).unwrap()["future"],
        "other-tab-new-key"
    );
}
#[test]
fn rehydration_changes_same_revision_bytes_and_old_native_write_receipt_cannot_acknowledge_them() {
    let mut state = Preferences::default();
    let generation = state.begin_hydration();
    state
        .hydrate(generation, Ok(Some(r#"{"future":"old"}"#)))
        .unwrap();
    state.patch(json!({"fontSizeCode":16})).unwrap();
    let in_flight = state.write_receipt().unwrap();
    let generation = state.begin_hydration();
    state
        .hydrate(
            generation,
            Ok(Some(
                r#"{"future":"new","newUnsupportedKey":{"keep":true}}"#,
            )),
        )
        .unwrap();
    let current = state.write_receipt().unwrap();
    assert_eq!(current.revision, in_flight.revision);
    assert_ne!(current.bytes, in_flight.bytes);
    state.acknowledge(&in_flight);
    assert_eq!(state.write_receipt(), Some(current.clone()));
    state.acknowledge(&current);
    assert!(state.write_receipt().is_none());
}
