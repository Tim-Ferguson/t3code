//! Behavioral boundaries whose meaning spans several individual wire fields.
use serde_json::{Value, json};
use t3_contracts::*;

#[test]
fn launch_and_execution_optional_nulls_have_separate_wire_and_service_views() {
    let launch:ThreadLaunchInput=serde_json::from_value(json!({"commandId":" command ","creationSource":null,"threadId":null,"reuseExistingThread":null,"generateTitle":null,"projectId":"project","title":" title ","modelSelection":{"provider":"codex","model":"model"},"runtimeMode":"full-access","interactionMode":"default","workspaceStrategy":{"type":"worktree","baseRef":" main ","branch":null,"startFromOrigin":null},"initialMessage":{"messageId":null,"text":"prompt","context":null,"attachments":[]}})).unwrap();
    let wire = serde_json::to_value(&launch).unwrap();
    assert!(wire.as_object().unwrap().contains_key("threadId"));
    assert_eq!(
        wire["modelSelection"],
        json!({"instanceId":"codex","model":"model"})
    );
    let service = launch.service_payload().unwrap();
    assert!(!service.as_object().unwrap().contains_key("threadId"));
    assert_eq!(service["title"], "title");
    assert_eq!(
        service["workspaceStrategy"],
        json!({"type":"worktree","baseRef":"main"})
    );
    assert_eq!(
        service["initialMessage"],
        json!({"text":"prompt","attachments":[]})
    );
    let respond:ProviderCommand=serde_json::from_value(json!({"type":"runtime-request.respond","commandId":"command","threadId":"thread","requestId":"request","decision":null,"answers":{"question":null},"attachmentsByQuestionId":null})).unwrap();
    let service = respond.service_payload().unwrap();
    assert!(!service.as_object().unwrap().contains_key("decision"));
    assert!(
        !service
            .as_object()
            .unwrap()
            .contains_key("attachmentsByQuestionId")
    );
    assert_eq!(service["answers"], json!({"question":null}));
}
fn attachment(kind: &str, mime: &str, size: u64) -> ChatAttachment {
    serde_json::from_value(
        json!({"type":kind,"id":"attachment","name":"file","mimeType":mime,"sizeBytes":size}),
    )
    .unwrap()
}
#[test]
fn aggregate_attachment_budget_counts_images_by_type_or_supported_mime() {
    let images = vec![attachment("image", "image/bmp", 10 * 1024 * 1024); 8];
    assert_eq!(get_provider_attachment_limit_error(&images), None);
    let mut excessive = images;
    excessive.push(attachment("image", "image/png", 1));
    assert!(
        get_provider_attachment_limit_error(&excessive)
            .unwrap()
            .contains("80 MiB")
    );
    let files = vec![attachment("file", "application/octet-stream", 50 * 1024 * 1024); 2];
    assert_eq!(get_provider_attachment_limit_error(&files), None);
    let disguised_images = vec![attachment("file", "IMAGE/PNG", 50 * 1024 * 1024); 2];
    assert!(get_provider_attachment_limit_error(&disguised_images).is_some());
    assert!(
        get_provider_attachment_limit_error(&vec![attachment("file", "text/plain", 1); 101])
            .unwrap()
            .contains("100 files")
    );
}
fn record() -> Value {
    json!({"version":1,"contextId":"record","kind":"image","label":"image","attachmentId":"attachment","name":"image.png","mimeType":"image/png","sizeBytes":1})
}
#[test]
fn composer_filters_bad_members_before_enforcing_unique_surviving_ids() {
    let valid = record();
    let context: OrchestrationMessageContext = serde_json::from_value(
        json!({"version":1,"records":[valid.clone(),{"contextId":"record","kind":"terminal"}]}),
    )
    .unwrap();
    assert_eq!(context.records.len(), 1);
    assert!(
        serde_json::from_value::<OrchestrationMessageContext>(
            json!({"version":1,"records":[valid.clone(),valid]})
        )
        .is_err()
    );
    assert!(
        serde_json::from_value::<OrchestrationMessageContext>(
            json!({"version":1,"records":vec![json!({"kind":"terminal"});201]})
        )
        .is_err()
    );
}
#[test]
fn known_events_fail_atomically_and_unknown_events_preserve_safe_resume_cursors() {
    assert!(serde_json::from_value::<ThreadStreamItem>(json!({"kind":"event","sequence":1,"event":{"id":"event","threadId":"thread","type":"run.updated","payload":{"id":"run"},"occurredAt":"2026-10-07T00:00:00Z"}})).is_err());
    let future: ThreadStreamItem = serde_json::from_str(
        "{\"kind\":\"event\",\"sequence\":1.0,\"event\":{\"type\":\"future.event\"}}",
    )
    .unwrap();
    assert!(matches!(
        future,
        ThreadStreamItem::UnknownEvent { sequence: 1, .. }
    ));
    assert!(serde_json::to_value(future).is_err());
    for sequence in [-1i64, 9_007_199_254_740_992] {
        assert!(
            serde_json::from_value::<ThreadStreamItem>(
                json!({"kind":"event","sequence":sequence,"event":{"type":"future.event"}})
            )
            .is_err()
        );
    }
    let future_item:ThreadStreamItem=serde_json::from_value(json!({"kind":"event","sequence":2,"event":{"type":"turn-item.updated","payload":{"type":42}}})).unwrap();
    assert!(matches!(
        future_item,
        ThreadStreamItem::UnknownEvent { sequence: 2, .. }
    ));
}
#[test]
fn limits_use_javascript_utf16_units_and_accept_integral_json_numbers() {
    assert!(serde_json::from_value::<BoundedString<2>>(json!("😀")).is_ok());
    assert!(serde_json::from_value::<BoundedString<2>>(json!("😀a")).is_err());
    assert_eq!(serde_json::from_str::<PositiveInt>("1.0").unwrap().0, 1);
    assert!(serde_json::from_str::<LiteralInt<1>>("1.0").is_ok());
    assert!(serde_json::from_str::<LiteralInt<1>>("1.5").is_err());
}
