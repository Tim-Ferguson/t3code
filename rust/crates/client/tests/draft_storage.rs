use serde_json::{Value, json};
use t3_client::draft_storage::{DraftStorage, DraftTarget};

const NOW: &str = "2026-10-08T12:00:00.000Z";
fn source() -> String {
    json!({"version":9,"state":{
    "draftsByThreadKey":{"a:thread":{"prompt":"Original A","attachments":[{"id":"image","name":"i.png","mimeType":"image/png","sizeBytes":1,"dataUrl":"data:image/png;base64,AA=="}],"unknownFutureContext":{"raw":"preserve"}},"b:thread":{"prompt":"Original B","attachments":[]}},
    "draftThreadsByThreadKey":{"local":{"threadId":"draft","environmentId":"a","projectId":"project","logicalProjectKey":"a:/workspace","runtimeMode":"approval-required","envMode":"worktree","branch":"main","createdAt":NOW}},
    "logicalProjectDraftThreadKeyByLogicalProjectKey":{"a:/workspace":"local"}
}}).to_string()
}

#[test]
fn source_envelope_errors_preserve_bytes_and_do_not_mutate_source() {
    for bytes in [
        "{broken".to_string(),
        json!({"version":10,"state":{}}).to_string(),
        json!({"version":"9","state":{}}).to_string(),
        json!({"version":9}).to_string(),
    ] {
        let mut storage = DraftStorage::default();
        storage.hydrate(Some(bytes.clone()), None, NOW);
        assert_eq!(storage.source_bytes.as_deref(), Some(bytes.as_str()));
        assert!(storage.recovery_error.is_some());
        assert!(storage.source_state.is_none());
        storage.edit_prompt(DraftTarget::thread("a", "thread"), "New unsent text".into());
        let (_, sidecar) = storage.prepare_write().unwrap().unwrap();
        assert!(!sidecar.contains("broken"));
        assert_eq!(storage.source_bytes.as_deref(), Some(bytes.as_str()));
    }
}

#[test]
fn early_edits_win_field_by_field_and_failed_writes_remain_dirty() {
    let target = DraftTarget::thread("a", "thread");
    let mut previous = DraftStorage::default();
    previous.edit_prompt(target.clone(), "Disk prompt".into());
    previous.edit_choices(target.clone(), json!({"modelSelection":null,"runtimeMode":"approval-required","environmentMode":null,"baseRef":""}));
    let (_, disk) = previous.prepare_write().unwrap().unwrap();
    let bytes = source();
    let mut storage = DraftStorage::default();
    storage.edit_prompt(target.clone(), String::new()); // An explicit empty edit wins.
    storage.hydrate(Some(bytes.clone()), Some(disk), NOW);
    assert_eq!(storage.prompt(&target), Some(""));
    assert_eq!(
        storage.changes(&target).unwrap().choices.as_ref().unwrap()["runtimeMode"],
        "approval-required"
    );
    assert_eq!(storage.source_bytes.as_deref(), Some(bytes.as_str()));
    let (first_revision, _) = storage.prepare_write().unwrap().unwrap();
    assert!(storage.dirty()); // A failed storage.setItem has no receipt.
    storage.edit_prompt(target.clone(), "Later edit".into());
    storage.write_succeeded(first_revision);
    assert!(storage.dirty());
    let (revision, sidecar) = storage.prepare_write().unwrap().unwrap();
    storage.write_succeeded(revision);
    assert!(!storage.dirty());
    let mut reloaded = DraftStorage::default();
    reloaded.hydrate(Some(bytes), Some(sidecar), NOW);
    assert_eq!(reloaded.prompt(&target), Some("Later edit"));
}

#[test]
fn acknowledgements_and_forget_survive_reload_without_cross_environment_leaks() {
    let a = DraftTarget::thread("a", "thread");
    let b = DraftTarget::thread("b", "thread");
    let bytes = source();
    let mut storage = DraftStorage::default();
    storage.hydrate(Some(bytes.clone()), None, NOW);
    assert_eq!(storage.prompt(&a), Some("Original A"));
    assert_eq!(storage.prompt(&b), Some("Original B"));
    assert!(
        storage
            .recovered_session(&DraftTarget::project("b", "project"))
            .is_none()
    );
    assert_eq!(
        storage
            .recovered_session(&DraftTarget::project("a", "project"))
            .unwrap()["runtimeMode"],
        "approval-required"
    );
    storage.acknowledge(a.clone());
    storage.forget("b");
    let (_, sidecar) = storage.prepare_write().unwrap().unwrap();
    let mut reload = DraftStorage::default();
    reload.hydrate(Some(bytes.clone()), Some(sidecar), NOW);
    assert!(reload.prompt(&a).is_none());
    assert!(reload.prompt(&b).is_none());
    assert_eq!(reload.source_bytes.as_deref(), Some(bytes.as_str()));
    reload.edit_prompt(a.clone(), "New text after acknowledged send".into());
    assert_eq!(reload.prompt(&a), Some("New text after acknowledged send"));
    assert!(reload.recovered(&a).is_none());
}

#[test]
fn unsupported_attachment_context_bytes_stay_identical_after_overlay_reload() {
    let bytes = source();
    let target = DraftTarget::thread("a", "thread");
    let mut storage = DraftStorage::default();
    storage.hydrate(Some(bytes.clone()), None, NOW);
    storage.edit_prompt(target.clone(), "Changed prompt only".into());
    let (_, overlay) = storage.prepare_write().unwrap().unwrap();
    let parsed: Value = serde_json::from_str(&overlay).unwrap();
    assert!(parsed.get("state").is_none());
    let mut reload = DraftStorage::default();
    reload.hydrate(Some(bytes.clone()), Some(overlay), NOW);
    assert_eq!(reload.source_bytes.as_deref(), Some(bytes.as_str()));
    assert_eq!(
        reload.recovered(&target).unwrap()["attachments"][0]["dataUrl"],
        "data:image/png;base64,AA=="
    );
    assert_eq!(reload.prompt(&target), Some("Changed prompt only"));
}

#[test]
fn malformed_or_future_overlay_never_overwrites_saved_overlay() {
    for bytes in ["{invalid".to_string(),json!({"version":2,"entries":[],"forgottenEnvironments":[]}).to_string(),json!({"version":1,"entries":[[{"environment":"a","kind":"thread","localId":"id"},{"acknowledged":false}],[{"environment":"a","kind":"thread","localId":"id"},{"acknowledged":false}]],"forgottenEnvironments":[]}).to_string()] {
        let mut storage=DraftStorage::default();storage.hydrate(Some(source()),Some(bytes),NOW);
        storage.edit_prompt(DraftTarget::thread("a","thread"),"New text".into());
        assert!(storage.prepare_write().is_err());assert!(storage.dirty());
    }
}

#[test]
fn early_forget_prevents_disk_overlay_resurrection_but_new_edits_are_owned() {
    let target = DraftTarget::thread("a", "thread");
    let mut disk = DraftStorage::default();
    disk.edit_prompt(target.clone(), "Forgotten saved prompt".into());
    let (_, sidecar) = disk.prepare_write().unwrap().unwrap();
    let mut storage = DraftStorage::default();
    storage.forget("a");
    storage.hydrate(Some(source()), Some(sidecar), NOW);
    assert!(storage.prompt(&target).is_none());
    assert!(storage.changes(&target).is_none());
    storage.edit_prompt(target.clone(), "New deliberate prompt".into());
    assert_eq!(storage.prompt(&target), Some("New deliberate prompt"));
    assert!(storage.recovered(&target).is_none());
}

#[test]
fn present_source_state_uses_original_recovery_even_when_not_an_object() {
    for state in [Value::Null, json!([]), json!(false), json!("legacy")] {
        let bytes = json!({"version":9,"state":state}).to_string();
        let mut storage = DraftStorage::default();
        storage.hydrate(Some(bytes.clone()), None, NOW);
        assert!(storage.recovery_error.is_none());
        assert_eq!(
            storage.source_state.as_deref().unwrap()["draftsByThreadKey"],
            json!({})
        );
        assert_eq!(storage.source_bytes.as_deref(), Some(bytes.as_str()));
    }
}

#[test]
fn accepted_thread_content_retains_latest_choices_across_reload_and_masks_source() {
    let target = DraftTarget::thread("a", "thread");
    let bytes = source();
    let mut storage = DraftStorage::default();
    storage.hydrate(Some(bytes.clone()), None, NOW);
    storage.edit_prompt(target.clone(), "Sent content".into());
    let choices = json!({"modelSelection":{"instanceId":"codex","model":"fixture-model","options":[{"id":"reasoningEffort","value":"low"}]},"runtimeMode":"approval-required","environmentMode":null,"baseRef":""});
    storage.edit_choices(target.clone(), choices.clone());
    storage.acknowledge_content(target.clone());
    assert!(storage.prompt(&target).is_none());
    assert!(storage.recovered(&target).is_none());
    let (_, sidecar) = storage.prepare_write().unwrap().unwrap();
    let mut reload = DraftStorage::default();
    reload.hydrate(Some(bytes.clone()), Some(sidecar), NOW);
    assert_eq!(reload.changes(&target).unwrap().choices, Some(choices));
    assert!(reload.prompt(&target).is_none());
    assert_eq!(reload.source_bytes.as_deref(), Some(bytes.as_str()));
    reload.edit_prompt(target.clone(), "Next message".into());
    assert_eq!(reload.prompt(&target), Some("Next message"));
    assert!(reload.recovered(&target).is_none());
}
