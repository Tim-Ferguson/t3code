//! The capability files claim what the Rust adapters do. They start from what the original
//! adapters declare (`generate_provider_capability_fixtures.mjs`) and may differ only by
//! turning off a capability the port has not ported; this list is that set, and a new difference
//! in either direction fails here.
use serde_json::Value;

const LACKING: &[(&str, &[&str])] = &[
    ("codex", &[
        "checkpointing.supportsNestedCheckpointScopes",
        "context.canGenerateSummaries",
        "sessions.supportsMultipleProviderThreadsPerSession",
        "subagents.canForkSubagentThread",
        "threads.canForkFromSubagentThread",
    ]),
    ("claudeAgent", &[
        "checkpointing.supportsNestedCheckpointScopes",
        "context.canGenerateSummaries",
        "tools.supportsDynamicToolCallbacks",
    ]),
    ("acpRegistry", &[
        "checkpointing.supportsNestedCheckpointScopes",
        "context.canGenerateSummaries",
    ]),
    // Grok is the ACP adapter plus its flavour: the same gaps, and the conversation snapshot the original
    // reads back. MCP tools are negotiated per session, as for any ACP agent (true when the thread has the T3
    // endpoint), and no Grok recording runs with one, so the default stays off.
    ("grok", &[
        "checkpointing.providerCanReadConversationSnapshot",
        "checkpointing.supportsNestedCheckpointScopes",
        "context.canGenerateSummaries",
        "threads.canReadThreadSnapshot",
        "tools.supportsMcpTools",
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
        ("grok", include_str!("../src/grok-capabilities.json")),
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

/// Antigravity is the ACP adapter plus its flavour. What the port lacks is what ACP lacks, and MCP tools, which
/// are negotiated per session (true when the thread has the T3 endpoint) and which no scripted Antigravity
/// session runs with, so the default stays off. The declared set is `generate_antigravity_capability_fixtures.mjs`'s.
const ANTIGRAVITY_LACKING: &[&str] = &[
    "checkpointing.supportsNestedCheckpointScopes",
    "context.canGenerateSummaries",
    "tools.supportsMcpTools",
];
#[test]
fn the_antigravity_capability_file_differs_from_the_original_only_where_the_port_lacks_a_feature() {
    let original: Value = serde_json::from_str(include_str!("fixtures/antigravity-capabilities-declared.json")).unwrap();
    let (mut declared, mut ported) = (vec![], vec![]);
    flat("", &original, &mut declared);
    flat("", &serde_json::from_str::<Value>(include_str!("../src/antigravity-capabilities.json")).unwrap(), &mut ported);
    assert_eq!(declared.len(), ported.len(), "the same capabilities are declared");
    for ((key, original), (ported_key, ported)) in declared.iter().zip(&ported) {
        assert_eq!(key, ported_key);
        if ANTIGRAVITY_LACKING.contains(&key.as_str()) {
            assert_eq!((original, ported), (&Value::Bool(true), &Value::Bool(false)), "antigravity {key} is documented as not ported");
        } else {
            assert_eq!(original, ported, "antigravity {key} differs from the original without being documented as not ported");
        }
    }
    // Every capability the port claims that the generic ACP adapter does not is one a scripted session covers.
    let acp: Value = serde_json::from_str(include_str!("../src/acp-capabilities.json")).unwrap();
    let mut generic = vec![];
    flat("", &acp, &mut generic);
    let claimed: Vec<&str> = ported.iter().zip(&generic).filter(|((_, ours), (_, theirs))| ours != theirs).map(|((key, _), _)| key.as_str()).collect();
    assert_eq!(claimed, ["sessions.supportsModelSwitchInSession", "sessions.supportsRuntimeModeSwitchInSession", "subagents.supportsSubagents"]);
}
