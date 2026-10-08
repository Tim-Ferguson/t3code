use serde_json::json;
use t3_contracts::*;
#[test]
fn provider_event_routing_and_task_identity_remain_separate() {
    let wire = json!({"eventId":" event ","provider":"codex","providerInstanceId":"codex-personal","threadId":" thread ","createdAt":"unchecked source timestamp","type":"task.updated","payload":{"taskId":" task ","status":"idle","agentKind":"background","agentId":"owner","parentAgentId":"parent","agentPath":"/root/agent","timelineBypass":true,"endedAt":"unchecked timestamp","runHandles":{"sessionUrl":"https://example.test/session","scriptPath":"/workflow.js"}},"raw":{"source":"codex.app-server.notification","method":"task/updated","payload":{"native_unrecognized":{"future":true}}}});
    let event: ProviderRuntimeEventV2 = serde_json::from_value(wire).unwrap();
    assert_eq!(
        event
            .base()
            .provider_instance_id
            .as_ref()
            .unwrap()
            .as_ref()
            .unwrap()
            .as_str(),
        "codex-personal"
    );
    let ProviderRuntimeEventV2::TaskUpdated(event) = event else {
        panic!("incorrect event dispatch")
    };
    assert_eq!(event.payload.agent_id.unwrap().unwrap().as_str(), "owner");
    assert_eq!(event.payload.timeline_bypass, Some(Some(true)));
    assert_eq!(event.payload.status, Some(Some(RuntimeTaskStatus::Idle)));
    assert!(
        event.base.raw.unwrap().unwrap().payload["native_unrecognized"]["future"]
            .as_bool()
            .unwrap()
    );
}
#[test]
fn individual_event_aliases_and_union_reject_wrong_or_unknown_discriminators() {
    let wire = json!({"eventId":"event","provider":"codex","threadId":"thread","createdAt":"time","type":"session.exited","payload":{}});
    assert!(serde_json::from_value::<ProviderRuntimeSessionExitedEvent>(wire.clone()).is_ok());
    assert!(serde_json::from_value::<ProviderRuntimeSessionStartedEvent>(wire.clone()).is_err());
    let mut future = wire;
    future["type"] = json!("future.event");
    assert!(serde_json::from_value::<ProviderRuntimeEventV2>(future).is_err());
    assert!(serde_json::from_value::<ProviderRuntimeEventV2>(json!([])).is_err());
}
#[test]
fn required_unknown_requires_presence_and_preserves_explicit_null() {
    assert!(serde_json::from_value::<RuntimeEventRaw>(json!({"source":"acp.jsonrpc"})).is_err());
    let raw: RuntimeEventRaw =
        serde_json::from_value(json!({"source":"acp.jsonrpc","payload":null})).unwrap();
    assert_eq!(
        serde_json::to_value(raw).unwrap(),
        json!({"source":"acp.jsonrpc","payload":null})
    );
    let optional: SessionStartedPayload = serde_json::from_value(json!({})).unwrap();
    assert_eq!(
        serde_json::to_value(optional).unwrap(),
        json!({}),
        "optional Unknown can be omitted while required Unknown cannot"
    );
}

#[test]
fn provider_question_options_preserve_empty_description_without_constructor_defaults() {
    let wire = json!({"questions":[{"id":" id ","header":" header ","question":" question ","options":[{"label":" label ","description":"","value":" raw "}]}]});
    let payload: UserInputRequestedPayload = serde_json::from_value(wire).unwrap();
    assert!(
        payload.questions[0].multi_select.is_none(),
        "source constructor default does not change JSON decode"
    );
    assert_eq!(payload.questions[0].options[0].description, "");
    assert_eq!(
        payload.questions[0].options[0].value,
        Some(Some(" raw ".into()))
    );
    assert!(serde_json::from_value::<UserInputQuestionV2>(json!({"id":"id","header":"header","question":"question","options":[{"label":"label","description":""}]})).is_err(),"orchestration display questions have a stricter normalized description");
}
#[test]
fn acp_raw_extension_is_open_but_requires_both_delimiters() {
    for source in [
        "acp..extension",
        "acp.custom-driver.extension",
        "acp.future/命名.extension",
    ] {
        assert!(serde_json::from_value::<RuntimeEventRawSource>(json!(source)).is_ok());
    }
    for source in [
        "acp.extension",
        "acp.custom.extension.extra",
        " codex.eventmsg ",
        "future.notification",
    ] {
        assert!(serde_json::from_value::<RuntimeEventRawSource>(json!(source)).is_err());
    }
}
