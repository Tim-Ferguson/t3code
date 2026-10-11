//! The reply check's decoding against the original's, over cases `tools/generate_rpc_response_cases.mjs` derives from the
//! schema of every reply in packages/contracts/src/rpc.ts: replies the original's encoder makes from generated values, and
//! mutations of them. The check must accept what the original's decoder accepts (with `onExcessProperty: "error"`) and refuse
//! what it refuses; the text of the refusal is compared too.
use crate::mcp_tool_input::decoder::{Js, response_schemas};
use crate::rpc_decode_tests::expand;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub(crate) struct Case {
    pub method: String,
    pub chunk: bool,
    pub input: Value,
    pub accepted: bool,
    pub message: Option<String>,
}
pub(crate) fn cases() -> Vec<Case> {
    include_str!("../tests/fixtures/rpc-response-cases.jsonl")
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let row: Value = serde_json::from_str(line).unwrap();
            Case { method: row["method"].as_str().unwrap().to_owned(), chunk: row["part"] == "chunk", input: expand(&row["input"]), accepted: row["accepted"] == true, message: row["message"].as_str().map(str::to_owned) }
        })
        .collect()
}

#[test]
fn the_check_agrees_with_the_original_decoder_on_every_reply() {
    let schemas = response_schemas();
    let mut counts: BTreeMap<String, [usize; 4]> = BTreeMap::new();
    let mut examples: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut seen_parts: BTreeSet<(String, bool)> = BTreeSet::new();
    for case in cases() {
        seen_parts.insert((case.method.clone(), case.chunk));
        let verdict = schemas.decode_response(&case.method, case.chunk, &Js::from_json(&case.input)).expect("a method with a reply schema");
        let entry = counts.entry(format!("{}{}", case.method, if case.chunk { " (chunk)" } else { "" })).or_default();
        entry[0] += 1;
        let shown = || case.input.to_string().chars().take(300).collect::<String>();
        let key = format!("{}{}", case.method, if case.chunk { " (chunk)" } else { "" });
        match (&verdict, case.accepted) {
            (Ok(_), false) => {
                entry[1] += 1;
                examples.entry(key).or_default().push(format!("accepted, original refuses ({}): {}", case.message.as_deref().unwrap_or_default().replace('\n', " / "), shown()));
            }
            (Err(issue), true) => {
                entry[2] += 1;
                examples.entry(key).or_default().push(format!("refused ({}), original accepts: {}", issue.format().replace('\n', " / "), shown()));
            }
            (Err(issue), false) if Some(issue.format()).as_deref() != case.message.as_deref() => {
                entry[3] += 1;
                examples.entry(key).or_default().push(format!("text differs: ours {:?}, original {:?}: {}", issue.format(), case.message.as_deref().unwrap_or_default(), shown()));
            }
            _ => {}
        }
    }
    if std::env::var("T3_PRINT_RESPONSE_MISMATCHES").is_ok() {
        for (method, [cases, accepts_refused, refuses_accepted, text]) in &counts {
            if accepts_refused + refuses_accepted + text > 0 {
                eprintln!("{method}: {cases} cases, {accepts_refused} accepted that the original refuses, {refuses_accepted} refused that it accepts, {text} refusals with other text");
                for example in examples.get(method).into_iter().flatten().take(4) {
                    eprintln!("    {example}");
                }
            }
        }
    }
    let wrong: Vec<_> = counts.iter().filter(|(_, [_, a, b, c])| a + b + c > 0).map(|(method, counts)| format!("{method}: {counts:?}")).collect();
    assert!(wrong.is_empty(), "{} replies differ: {wrong:#?}", wrong.len());
    // Every reply schema of the original's group is exercised: methods that share one share cases.
    let covered: BTreeSet<&str> = seen_parts.iter().map(|(method, _)| method.as_str()).collect();
    assert!(covered.len() > 100, "{} methods have cases", covered.len());
}

/// Every method of the original's group has a reply schema, and the functions its schemas hold are ported.
#[test]
fn every_method_has_a_reply_schema_and_every_function_is_ported() {
    let schemas = response_schemas();
    let methods: BTreeSet<&str> = schemas.response_methods().map(|(method, _)| method).collect();
    let payload_methods: BTreeSet<&str> = crate::mcp_tool_input::decoder::rpc_methods().collect();
    assert_eq!(methods, payload_methods);
    let mut known: Vec<&str> = crate::mcp_tool_functions::FILTERS.iter().chain(crate::mcp_tool_functions::TRANSFORMS).chain(crate::mcp_tool_functions::RESPONSE_FILTERS).chain(crate::mcp_tool_functions::RESPONSE_TRANSFORMS).copied().collect();
    known.sort_unstable();
    known.dedup();
    let mut held: Vec<&str> = crate::mcp_tool_input::decoder::response_function_hashes().iter().map(|(hash, _, _)| *hash).collect();
    held.sort_unstable();
    assert!(held.iter().all(|hash| known.contains(hash)), "a function the reply schemas hold is not ported: regenerate with tools/generate_mcp_tool_schemas.mjs");
}

fn exit(value: Value) -> t3_contracts::RpcServerMessage {
    t3_contracts::RpcServerMessage::Exit { request_id: "1".into(), exit: serde_json::from_value(value).unwrap() }
}

/// What the transport hook reports, on replies built the way a handler builds them.
#[test]
fn a_reply_the_schema_does_not_encode_is_reported_with_its_path_and_the_librarys_text() {
    use crate::rpc_response::violations;
    let good = serde_json::json!({"_tag":"Success","value":{"relativeUrl":"a.png","expiresAt":5}});
    assert_eq!(violations("assets.createUrl", &exit(good.clone())), vec![]);
    // A key the schema does not name, a missing key, a blank where the schema trims and checks, a typed error of another method.
    let extra = serde_json::json!({"_tag":"Success","value":{"relativeUrl":"a.png","expiresAt":5,"operation":"x"}});
    let found = violations("assets.createUrl", &exit(extra));
    assert_eq!((found[0].path.as_str(), found[0].message.as_str()), ("[\"value\"][\"operation\"]", "Expected no excess property"));
    let missing = serde_json::json!({"_tag":"Success","value":{"relativeUrl":"a.png"}});
    assert_eq!(violations("assets.createUrl", &exit(missing))[0].message, "Missing key");
    let blank = serde_json::json!({"_tag":"Success","value":{"relativeUrl":" ","expiresAt":5}});
    assert_eq!(violations("assets.createUrl", &exit(blank))[0].message, "Expected a non-blank string");
    let wrong_error = serde_json::json!({"_tag":"Failure","cause":[{"_tag":"Fail","error":{"_tag":"NoSuchError"}}]});
    assert!(!violations("assets.createUrl", &exit(wrong_error)).is_empty());
    // A defect is a string, an interrupt carries its fiber.
    assert_eq!(violations("assets.createUrl", &exit(serde_json::json!({"_tag":"Failure","cause":[{"_tag":"Die","defect":"boom"},{"_tag":"Interrupt","fiberId":1}]}))), vec![]);
    assert!(!violations("assets.createUrl", &exit(serde_json::json!({"_tag":"Failure","cause":[{"_tag":"Interrupt"}]}))).is_empty());
}

/// A stream's chunks are a non-empty array of the stream's element.
#[test]
fn a_chunk_is_checked_against_the_element_of_the_stream() {
    use crate::rpc_response::violations;
    let chunk = |values: Vec<Value>| t3_contracts::RpcServerMessage::Chunk { request_id: "1".into(), values };
    assert_eq!(violations("subscribeBackgroundPolicy", &chunk(vec![serde_json::json!({"nonsense":true})])).is_empty(), false);
}

/// A server without a resource monitor answers the resource-telemetry methods with `NativeServiceUnavailableError`, which the
/// original's contract (`EnvironmentAuthorizationError` alone) does not name: let through there, in that shape, and nowhere else.
#[test]
fn an_unconfigured_resource_monitor_is_let_through_for_the_telemetry_methods_only() {
    use crate::rpc_response::violations;
    let unavailable = |error: Value| serde_json::json!({"_tag":"Failure","cause":[{"_tag":"Fail","error":error}]});
    let native = unavailable(serde_json::json!({"_tag":"NativeServiceUnavailableError","message":"Resource telemetry is not configured."}));
    for method in ["server.getResourceTelemetryHistory", "server.retryResourceTelemetry", "subscribeResourceTelemetry"] {
        assert_eq!(violations(method, &exit(native.clone())), vec![], "{method}");
        // Another shape of the error is not let through.
        assert!(!violations(method, &exit(unavailable(serde_json::json!({"_tag":"NativeServiceUnavailableError","message":"x","detail":1})))).is_empty(), "{method}");
        assert!(!violations(method, &exit(unavailable(serde_json::json!({"_tag":"NativeServiceUnavailableError","message":5})))).is_empty(), "{method}");
        assert!(!violations(method, &exit(unavailable(serde_json::json!({"_tag":"NativeMethodUnsupportedError","message":"x"})))).is_empty(), "{method}");
    }
    // Other methods keep the original's contract.
    assert!(!violations("server.getHostResources", &exit(native.clone())).is_empty());
    assert!(!violations("assets.createUrl", &exit(native)).is_empty());
}
