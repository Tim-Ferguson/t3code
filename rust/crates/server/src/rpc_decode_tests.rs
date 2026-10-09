//! The port's decoding of ten WebSocket RPC payloads, against the original's own decoding of cases
//! `tools/generate_rpc_decode_cases.mjs` derives from each payload schema in
//! packages/contracts/src/rpc.ts. What matters most is a payload the original refuses and the port
//! carries out; the other direction is counted too. Each method's decoder is the one its handler
//! runs before it does anything.
use serde_json::Value;
use std::collections::BTreeMap;

/// The generator writes a long run of one string as `{"$repeat": [unit, count]}`, and a long array
/// of one element as `{"$items": [element, count]}`.
fn expand(value: &Value) -> Value {
    match value {
        Value::Object(object) if object.len() == 1 && object.contains_key("$repeat") => Value::String(object["$repeat"][0].as_str().unwrap().repeat(object["$repeat"][1].as_u64().unwrap() as usize)),
        Value::Object(object) if object.len() == 1 && object.contains_key("$items") => Value::Array(vec![expand(&object["$items"][0]); object["$items"][1].as_u64().unwrap() as usize]),
        Value::Object(object) => Value::Object(object.iter().map(|(key, found)| (key.clone(), expand(found))).collect()),
        Value::Array(items) => Value::Array(items.iter().map(expand).collect()),
        other => other.clone(),
    }
}

fn typed<T: serde::de::DeserializeOwned>(payload: Value) -> bool {
    serde_json::from_value::<T>(payload).is_ok()
}

/// Whether the port's handler for `method` gets past decoding `payload`.
fn port_accepts(method: &str, payload: Value) -> bool {
    use t3_contracts::*;
    // The original's decoding runs first, at the transport (`rpc_admission.rs`); then the handler's own.
    if crate::rpc_admission::refuse(method, &payload).is_some() {
        return false;
    }
    match method {
        "vcs.listRefs" => crate::vcs_input::list_refs_input(&payload).is_ok(),
        "projects.createNew" => crate::project_rpc::decode_create_new(payload).is_ok(),
        "scheduledTasks.upsert" => typed::<ScheduledTaskUpsertInput>(payload),
        "pullRequests.comment" => crate::pull_request_rpc::decode_comment(payload).is_ok(),
        "provider.install.start" => typed::<ProviderSetupInput>(payload),
        "orchestration.searchThreads" => typed::<OrchestrationSearchThreadsInput>(payload),
        "git.runStackedAction" => crate::git_workflow_rpc::decode_stacked_input(payload).is_ok(),
        "server.upsertKeybinding" => typed::<ServerUpsertKeybindingInput>(payload),
        "attachments.createUploadUrl" => typed::<AttachmentCreateUploadUrlInput>(payload),
        "sourceControl.cloneRepository" => typed::<SourceControlCloneRepositoryInput>(payload),
        other => panic!("{other}"),
    }
}

/// Per method: payloads the port carries out that the original refuses, and payloads it refuses that the original accepts.
#[test]
fn the_port_refuses_the_rpc_payloads_the_original_refuses() {
    let mut counts: BTreeMap<String, (usize, usize, usize)> = BTreeMap::new();
    let mut examples: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for line in include_str!("../tests/fixtures/rpc-decode-cases.jsonl").lines().filter(|line| !line.is_empty()) {
        let row: Value = serde_json::from_str(line).unwrap();
        let method = row["method"].as_str().unwrap();
        let (input, original) = (expand(&row["input"]), row["accepted"] == true);
        let port = port_accepts(method, input.clone());
        let entry = counts.entry(method.to_owned()).or_default();
        entry.0 += 1;
        if port && !original {
            entry.1 += 1;
            examples.entry(method.to_owned()).or_default().push(format!("carried out, original refuses: {}", input.to_string().chars().take(200).collect::<String>()));
        } else if !port && original {
            entry.2 += 1;
            examples.entry(method.to_owned()).or_default().push(format!("refused, original accepts: {}", input.to_string().chars().take(200).collect::<String>()));
        }
    }
    if std::env::var("T3_PRINT_RPC_MISMATCHES").is_ok() {
        for (method, (cases, accepts_refused, refuses_accepted)) in &counts {
            eprintln!("{method}: {cases} cases, {accepts_refused} carried out that the original refuses, {refuses_accepted} refused that the original accepts");
            for example in examples.get(method).into_iter().flatten().take(12) {
                eprintln!("    {example}");
            }
        }
    }
    let wrong: Vec<_> = counts.iter().filter(|(_, (_, accepts_refused, refuses_accepted))| accepts_refused + refuses_accepted > 0).map(|(method, counts)| format!("{method}: {counts:?}")).collect();
    assert!(wrong.is_empty(), "{wrong:#?}");
    assert_eq!(counts.len(), 10);
    // The methods the transport checks are the ones these cases cover.
    let mut covered: Vec<&str> = crate::mcp_tool_input::decoder::rpc_methods().collect();
    covered.sort_unstable();
    assert_eq!(covered, counts.keys().map(String::as_str).collect::<Vec<_>>());
}
