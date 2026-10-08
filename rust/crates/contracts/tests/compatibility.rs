use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use t3_contracts::*;

#[test]
fn identifiers_use_ecmascript_whitespace_and_reject_blank_ids() {
    for raw in [
        "  thread-1\n",
        "\u{feff}thread-1\u{00a0}",
        "\u{2028}thread-1\u{3000}",
    ] {
        let id: ThreadId = serde_json::from_value(json!(raw)).unwrap();
        assert_eq!(id.as_str(), "thread-1");
        assert_eq!(serde_json::to_value(id).unwrap(), "thread-1");
    }
    for raw in ["", "\u{feff} \n", "\u{2029}"] {
        assert!(ThreadId::new(raw).is_err());
    }
    // U+0085 is Rust whitespace, but ECMAScript does not trim it.
    assert_eq!(
        ThreadId::new("\u{0085}a\u{0085}").unwrap().as_str(),
        "\u{0085}a\u{0085}"
    );
}

#[test]
fn provider_slugs_are_open_but_still_validated() {
    for slug in [
        "codex",
        "codex_personal",
        "codex-work",
        "claudeAgent",
        "ollama",
        "abc123",
    ] {
        assert!(ProviderDriverKind::new(slug).is_ok());
        assert!(ProviderInstanceId::new(slug).is_ok());
    }
    for slug in [
        "",
        "1codex",
        "-codex",
        "_codex",
        "codex personal",
        "codex.personal",
        "codex/personal",
        "é",
    ] {
        assert!(ProviderInstanceId::new(slug).is_err());
    }
    assert!(ProviderInstanceId::new("a".repeat(64)).is_ok());
    assert!(ProviderInstanceId::new("a".repeat(65)).is_err());
}

#[test]
fn legacy_model_selection_is_canonicalized_and_explicit_routing_wins() {
    let legacy = json!({"provider":"ollama","model":" llama3:70b ","options":{"effort":" max ","fastMode":true,"emptyStr":" ","nullish":null,"nested":{"x":1}}});
    let selection: ModelSelection = serde_json::from_value(legacy).unwrap();
    assert_eq!(
        serde_json::to_value(selection).unwrap(),
        json!({"instanceId":"ollama","model":"llama3:70b","options":[{"id":"effort","value":"max"},{"id":"fastMode","value":true}]})
    );
    let explicit: ModelSelection = serde_json::from_value(
        json!({"provider":"codex","instanceId":"codex_personal","model":"x"}),
    )
    .unwrap();
    assert_eq!(explicit.instance_id.as_str(), "codex_personal");
    for bad in [
        json!({"provider":"codex","instanceId":"1bad","model":"x"}),
        json!({"provider":"codex","instanceId":null,"model":"x"}),
        json!({"instanceId":"codex","model":"x","options":null}),
    ] {
        assert!(serde_json::from_value::<ModelSelection>(bad).is_err());
    }
}

#[test]
fn canonical_options_reject_malformed_known_entries() {
    assert!(
        serde_json::from_value::<ModelSelection>(
            json!({"instanceId":"codex","model":"x","options":[{"id":"effort","value":42}]})
        )
        .is_err()
    );
    let parsed: ModelSelection = serde_json::from_value(
        json!({"instanceId":"codex","model":"x","options":[{"id":" fastMode ","value":false}]}),
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(parsed).unwrap()["options"],
        json!([{"id":"fastMode","value":false}])
    );
}

#[test]
fn explicit_granular_permissions_prevent_legacy_authorization_fallback() {
    use AuthEnvironmentScope::*;
    let session: SessionGrantInput = serde_json::from_value(json!({"authenticated":true,"scopes":["orchestration:operate"],"permissions":["future:permission"]})).unwrap();
    assert_eq!(session.permissions, Some(vec![]));
    assert!(!session_grants_scope(&session, OrchestrationOperate));
    assert!(!session_grants_scope(&session, SettingsWrite));
    let old: SessionGrantInput = serde_json::from_value(
        json!({"authenticated":true,"scopes":["orchestration:operate"],"auth":{}}),
    )
    .unwrap();
    assert!(session_grants_scope(&old, SettingsWrite));
    let granular: SessionGrantInput = serde_json::from_value(json!({"authenticated":true,"scopes":["orchestration:operate"],"auth":{"serverUpdateScope":"environment:maintain"}})).unwrap();
    assert!(!session_grants_scope(&granular, SettingsWrite));
    assert!(!ReviewWrite.is_grantable());
    assert!(
        auth_scope_required_response(FilesystemWrite)
            .required_scope
            .is_legacy()
    );
}

#[test]
fn forward_union_drops_only_unknown_tags_and_fails_malformed_known_tags() {
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    #[serde(tag = "type")]
    enum Sample {
        #[serde(rename = "known")]
        Known { required: String },
    }
    let good = decode_forward_union_array::<Sample>(
        json!([{"type":"future"},{"type":"known","required":"yes"}]),
        "type",
        &["known"],
    )
    .unwrap();
    assert_eq!(
        good,
        vec![Sample::Known {
            required: "yes".into()
        }]
    );
    for bad in [json!([{"type":"known"}]), json!([{}]), json!([{"type":42}])] {
        assert!(decode_forward_union_array::<Sample>(bad, "type", &["known"]).is_err());
    }
}

#[test]
fn unknown_thread_events_advance_the_cursor_but_cannot_be_sent() {
    let future: ThreadStreamItem = serde_json::from_value(
        json!({"kind":"event","sequence":2,"event":{"type":"run.from-a-future-server"}}),
    )
    .unwrap();
    assert_eq!(
        future,
        ThreadStreamItem::UnknownEvent {
            sequence: 2,
            event_type: "run.from-a-future-server".into()
        }
    );
    assert!(serde_json::to_value(future).is_err());
    let event = |payload: Value| json!({"kind":"event","sequence":3,"event":{"id":"event-3","type":"provider-session.detached","threadId":"thread-1","occurredAt":"2026-01-01T00:00:00Z","payload":payload}});
    assert!(serde_json::from_value::<ThreadStreamItem>(event(json!({}))).is_err());
    assert!(matches!(
        serde_json::from_value::<ThreadStreamItem>(event(
            json!({"providerSessionId":"session-1","detachedAt":"2026-01-01T00:00:00Z"})
        ))
        .unwrap(),
        ThreadStreamItem::Event { sequence: 3, .. }
    ));
}

#[test]
fn effect_rpc_wire_roundtrips_requests_chunks_and_failures() {
    let request = json!({"_tag":"Request","id":7,"tag":"orchestration.subscribeShell","payload":{},"headers":[],"traceId":"trace","isNotification":true});
    let decoded: RpcClientMessage = serde_json::from_value(request.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), request);
    let failure = json!({"_tag":"Exit","requestId":"7","exit":{"_tag":"Failure","cause":[{"_tag":"Fail","error":{"_tag":"NotFound","message":"missing"}},{"_tag":"Die","defect":"boom"},{"_tag":"Interrupt","fiberId":3}]}});
    let decoded: RpcServerMessage = serde_json::from_value(failure.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), failure);
    assert!(
        serde_json::from_value::<RpcServerMessage>(
            json!({"_tag":"Chunk","requestId":1,"values":[]})
        )
        .is_err()
    );
    assert!(serde_json::from_value::<RpcClientMessage>(json!({"_tag":"Request","id":"1","tag":"x","payload":{},"headers":[],"isNotification":false})).is_err());
}

#[test]
fn project_fields_preserve_future_payloads_and_required_nullability() {
    let project = json!({"id":"p1","title":" Project ","workspaceRoot":" /workspace ","defaultModelSelection":null,"scripts":[],"createdAt":"unvalidated-iso-string","updatedAt":"later","repositoryIdentity":{"newField":"future"}});
    let decoded: ProjectShell = serde_json::from_value(project.clone()).unwrap();
    assert_eq!(decoded.title.as_str(), "Project");
    assert_eq!(
        serde_json::to_value(decoded).unwrap()["repositoryIdentity"],
        project["repositoryIdentity"]
    );
    let mut missing = project;
    missing
        .as_object_mut()
        .unwrap()
        .remove("defaultModelSelection");
    assert!(serde_json::from_value::<ProjectShell>(missing).is_err());
    assert!(serde_json::from_value::<ShellSnapshot>(json!({"schemaVersion":0,"snapshotSequence":0,"projects":[],"threads":[],"archivedThreads":[]})).is_err());
}

#[test]
fn default_configuration_matches_required_native_boundaries() {
    let defaults = ServerSettings::default();
    assert_eq!(defaults.providers.codex.binary_path.as_str(), "codex");
    assert!(defaults.providers.codex.enabled);
    assert!(!defaults.providers.cursor.enabled);
    assert_eq!(default_resolved_keybindings().0.len(), 80);
    let mixed: ServerSettingsPatch = serde_json::from_value(
        serde_json::json!({"providers":{},"enableAgentBrowserAccess":false}),
    )
    .unwrap();
    assert_eq!(
        mixed.required_scopes(),
        vec![
            AuthEnvironmentScope::SettingsWrite,
            AuthEnvironmentScope::ProvidersManage
        ]
    );
    let providers: ServerSettingsPatch =
        serde_json::from_value(serde_json::json!({"providerInstances":{}})).unwrap();
    assert_eq!(
        providers.required_scopes(),
        vec![AuthEnvironmentScope::ProvidersManage]
    );
    assert_eq!(
        serde_json::from_value::<ServerSettingsPatch>(serde_json::json!({}))
            .unwrap()
            .required_scopes(),
        vec![AuthEnvironmentScope::SettingsWrite]
    );
}
