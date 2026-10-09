//! The capability files claim what the Rust adapters do. They start from what the original
//! adapters declare (`generate_provider_capability_fixtures.mjs`) and may differ only by
//! turning off a capability the port has not ported; this list is that set, and a new difference
//! in either direction fails here.
use serde_json::Value;

const LACKING: &[(&str, &[&str])] = &[
    ("codex", &[
        "checkpointing.supportsNestedCheckpointScopes",
        "context.acceptsSyntheticUserContext",
        "context.canConsumeHandoffSummaries",
        "context.canGenerateSummaries",
        "context.supportsDeltaHandoff",
        "context.supportsFullThreadHandoff",
        "sessions.supportsMultipleProviderThreadsPerSession",
        "sessions.supportsProviderSwitchingViaHandoff",
        "subagents.canForkSubagentThread",
        "threads.canForkFromSubagentThread",
        "threads.canForkFromTurn",
        "threads.canForkThread",
    ]),
    ("claudeAgent", &[
        "checkpointing.supportsNestedCheckpointScopes",
        "context.acceptsSyntheticUserContext",
        "context.canConsumeHandoffSummaries",
        "context.canGenerateSummaries",
        "context.supportsDeltaHandoff",
        "context.supportsFullThreadHandoff",
        "sessions.supportsProviderSwitchingViaHandoff",
        "subagents.emitsSubagentLifecycle",
        "subagents.supportsSubagents",
        "threads.canForkFromTurn",
        "threads.canForkThread",
        "tools.supportsDynamicToolCallbacks",
    ]),
    ("acpRegistry", &[
        "checkpointing.supportsNestedCheckpointScopes",
        "context.canConsumeHandoffSummaries",
        "context.canGenerateSummaries",
        "context.supportsDeltaHandoff",
        "context.supportsFullThreadHandoff",
        "sessions.supportsProviderSwitchingViaHandoff",
    ]),
];

fn flat(prefix: &str, value: &Value, out: &mut Vec<(String, Value)>) {
    for (key, value) in value.as_object().unwrap() {
        match value {
            Value::Object(_) => flat(&format!("{prefix}{key}."), value, out),
            _ => out.push((format!("{prefix}{key}"), value.clone())),
        }
    }
}

#[test]
fn the_capability_files_differ_from_the_original_only_where_the_port_lacks_a_feature() {
    let original: Value = serde_json::from_str(include_str!("fixtures/provider-capabilities.json")).unwrap();
    let files = [
        ("codex", include_str!("../src/codex-capabilities.json")),
        ("claudeAgent", include_str!("../src/claude-capabilities.json")),
        ("acpRegistry", include_str!("../src/acp-capabilities.json")),
    ];
    for (driver, file) in files {
        let (mut declared, mut ported) = (vec![], vec![]);
        flat("", &original[driver], &mut declared);
        flat("", &serde_json::from_str::<Value>(file).unwrap(), &mut ported);
        let lacking = LACKING.iter().find(|(name, _)| *name == driver).unwrap().1;
        assert_eq!(declared.len(), ported.len(), "{driver}: the same capabilities are declared");
        for ((key, original), (ported_key, ported)) in declared.iter().zip(&ported) {
            assert_eq!(key, ported_key);
            if lacking.contains(&key.as_str()) {
                assert_eq!((original, ported), (&Value::Bool(true), &Value::Bool(false)), "{driver} {key} is documented as not ported");
            } else {
                assert_eq!(original, ported, "{driver} {key} differs from the original without being documented as not ported");
            }
        }
    }
}
