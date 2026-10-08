use serde_json::json;
use t3_client::work_log::{
    detail_revision, format_value, needs_detail, output_text, overlay_detail,
};

#[test]
fn detail_cache_coalesces_live_updates_but_refreshes_finished_items() {
    for status in ["idle", "pending", "running", "waiting"] {
        assert_eq!(
            detail_revision(&json!({"status":status,"updatedAt":"changing"})),
            "live"
        );
    }
    assert_eq!(
        detail_revision(&json!({"status":"completed","updatedAt":"final"})),
        "final"
    );
    assert!(needs_detail(
        &json!({"type":"dynamic_tool","input":{"summary":"large args","truncated":true}})
    ));
    assert!(needs_detail(
        &json!({"type":"command_execution","outputOmitted":true})
    ));
    assert!(!needs_detail(
        &json!({"type":"reasoning","outputOmitted":true})
    ));
}

#[test]
fn legacy_claude_command_output_unwraps_only_complete_stdout_stderr_envelopes() {
    let raw = r#"{"stdout":"success\n","stderr":"warning","interrupted":false}"#;
    assert_eq!(
        output_text(&json!({"type":"command_execution","output":raw})),
        Some("success\n\nwarning".into())
    );
    let raw = r#"{"stdout":"message","custom":true}"#;
    assert_eq!(
        output_text(&json!({"type":"command_execution","output":raw})),
        Some(raw.into())
    );
    assert_eq!(
        output_text(&json!({"type":"command_execution","output":"  "})),
        None
    );
}

#[test]
fn tool_content_blocks_prefer_text_and_preserve_structured_fallback() {
    assert_eq!(
        format_value(
            &json!({"content":[{"type":"text","text":"Captured the home screen."},{"type":"image","mimeType":"image/png"}]})
        ),
        Some("Captured the home screen.".into())
    );
    assert_eq!(
        format_value(&json!({"content":[{"type":"image","mimeType":"image/svg+xml"}]})),
        Some("[image]".into())
    );
    assert_eq!(
        format_value(&json!({"content":[{"type":"image","mimeType":"image/png"}]})),
        None
    );
    assert_eq!(
        format_value(
            &json!({"content":[{"type":"text","text":"result"}],"structuredContent":{"duplicated":"result"}})
        ),
        Some("result".into())
    );
    assert_eq!(
        format_value(&json!({"content":[],"structuredContent":{"count":1}})),
        Some("{\n  \"content\": [],\n  \"structuredContent\": {\n    \"count\": 1\n  }\n}".into())
    );
    assert_eq!(
        format_value(&json!("{\"one\":1}\n{\"two\":2}")),
        Some("{\n  \"one\": 1\n}\n\n{\n  \"two\": 2\n}".into())
    );
    assert_eq!(
        output_text(
            &json!({"type":"dynamic_tool","outputOmitted":true,"output":{"text":"partial"}})
        ),
        None
    );
}

#[test]
fn search_output_keeps_locations_and_uses_url_when_title_is_absent() {
    assert_eq!(
        output_text(
            &json!({"type":"file_search","results":[{"fileName":"src/main.rs","line":12,"preview":" fn main() "}]})
        ),
        Some("src/main.rs:12\nfn main()".into())
    );
    assert_eq!(
        output_text(
            &json!({"type":"web_search","results":[{"url":"https://example.test","title":" Reference ","snippet":" relevant "},{"url":"https://other.test"}]})
        ),
        Some("Reference\nhttps://example.test\nrelevant\n\nhttps://other.test".into())
    );
}

#[test]
fn fetched_detail_cannot_overwrite_live_status_or_another_item() {
    let projected = json!({"id":"item","type":"command_execution","status":"completed","updatedAt":"new","input":"ls","outputOmitted":true});
    let fetched = json!({"id":"item","type":"command_execution","status":"running","updatedAt":"old","input":"ls","output":"file"});
    let item = overlay_detail(&projected, &fetched);
    assert_eq!(item["status"], "completed");
    assert_eq!(item["updatedAt"], "new");
    assert_eq!(item["output"], "file");
    assert!(item.get("outputOmitted").is_none());
    assert_eq!(
        overlay_detail(
            &projected,
            &json!({"id":"different","type":"command_execution","output":"private"})
        ),
        projected
    );
}

#[test]
fn forgetting_environment_removes_every_alias_and_all_authorization_and_drafts() {
    use t3_client::{connection::EnvironmentEndpoint, environments::EnvironmentCatalog};
    use t3_contracts::{AuthEnvironmentScope, EnvironmentId, SessionGrantInput};
    let mut catalog = EnvironmentCatalog::default();
    let old = serde_json::from_value::<EnvironmentId>(json!("old-env")).unwrap();
    let new = serde_json::from_value::<EnvironmentId>(json!("new-env")).unwrap();
    for address in ["http://lan.test", "https://relay.test/proxy"] {
        catalog
            .register(
                &EnvironmentEndpoint::new(address).unwrap(),
                old.clone(),
                "Old".into(),
                2,
            )
            .unwrap();
    }
    catalog
        .set_session(
            &old,
            serde_json::from_value::<SessionGrantInput>(
                json!({"authenticated":true,"scopes":["orchestration:operate"]}),
            )
            .unwrap(),
        )
        .unwrap();
    catalog
        .records
        .get_mut(&old)
        .unwrap()
        .cache
        .drafts
        .insert("thread".into(), "private".into());
    assert!(
        catalog
            .validate_identity(
                &EnvironmentEndpoint::new("http://lan.test").unwrap(),
                &new,
                2
            )
            .is_err()
    );
    assert!(catalog.forget(&old));
    assert!(!catalog.forget(&old));
    assert!(!catalog.allows(&old, AuthEnvironmentScope::OrchestrationOperate));
    for address in ["http://lan.test", "https://relay.test/proxy"] {
        catalog
            .register(
                &EnvironmentEndpoint::new(address).unwrap(),
                new.clone(),
                "New".into(),
                2,
            )
            .unwrap();
    }
    assert!(catalog.records[&new].cache.drafts.is_empty());
    assert!(!catalog.allows(&new, AuthEnvironmentScope::OrchestrationOperate));
}

#[test]
fn rpc_heartbeat_requires_pong_and_retry_delays_match_effect_policy() {
    use t3_client::connection::{Heartbeat, HeartbeatAction, reconnect_delay_ms};
    let mut heartbeat = Heartbeat::default();
    assert_eq!(heartbeat.tick(), HeartbeatAction::Ping);
    assert_eq!(heartbeat.tick(), HeartbeatAction::Timeout);
    heartbeat.reset();
    assert_eq!(heartbeat.tick(), HeartbeatAction::Ping);
    heartbeat.pong();
    assert_eq!(heartbeat.tick(), HeartbeatAction::Ping);
    assert_eq!(
        (0..4).map(reconnect_delay_ms).collect::<Vec<_>>(),
        vec![500, 750, 1125, 1688]
    );
    assert_eq!(reconnect_delay_ms(99), 5000);
}
