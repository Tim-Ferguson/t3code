//! The check of what the server sends. In the original every success value, typed error and stream chunk is encoded through the
//! schema `Rpc.make` names for it, so a client can rely on the exact shape; here replies are built by hand. With the
//! `outbound-check` feature (on for every test, never in a build of the server) the transport hands each `Exit` and `Chunk` it
//! writes to `inspect`, which decodes it as the client does (`tools/generate_mcp_tool_schemas.mjs` writes the schemas, from the
//! exit codec `RpcServer` encodes with and, for a stream, the codec of a non-empty array of its element) and refuses a key the
//! schema does not name, as nothing an encoder makes has one.
//!
//! A violation is recorded with the method, the path and the library's message. By default it also panics, which ends the
//! connection of the test that provoked it; `T3_OUTBOUND_CHECK=record` only records, and `T3_OUTBOUND_VIOLATIONS_FILE` names a file
//! every reply and violation is appended to as a JSON line (`{"method","part"}`, `{"method","part","path","message","actual"}`).
use crate::mcp_tool_input::decoder::{Issue, Js, Key, response_schemas};
use serde_json::{Value, json};
use std::{collections::HashMap, io::Write, sync::{Arc, Mutex}};
use t3_contracts::{RpcRequestId, RpcServerMessage};

/// One reply that is not a valid encoding.
#[derive(Debug, Clone, PartialEq)]
pub struct Violation {
    pub method: String,
    /// "exit" or "chunk".
    pub part: &'static str,
    /// Where in the exit (or the array of chunk values) the value sits, as the default formatter writes it.
    pub path: String,
    pub message: String,
    /// The value found there, cut short.
    pub actual: String,
}

pub type Methods = Arc<Mutex<HashMap<RpcRequestId, String>>>;

/// Every violation of a reply to `method`; none for a message that is not an exit or a chunk, and for a method the original's group
/// does not have (`has_schema`).
pub fn violations(method: &str, message: &RpcServerMessage) -> Vec<Violation> {
    let (part, wire) = match message {
        RpcServerMessage::Exit { exit, .. } => ("exit", serde_json::to_value(exit).unwrap_or(Value::Null)),
        RpcServerMessage::Chunk { values, .. } => ("chunk", Value::Array(values.clone())),
        _ => return Vec::new(),
    };
    if is_unconfigured_service_extension(method, &wire) {
        return Vec::new();
    }
    let schemas = response_schemas();
    // A reply can nest as deeply as a thread does, and the interpreter is recursive: a thread of its own with room for it.
    let outcome = std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(64 << 20)
            .spawn_scoped(scope, || schemas.decode_response(method, part == "chunk", &Js::from_json(&wire)))
            .expect("a thread for the reply check")
            .join()
            .expect("the reply check does not panic")
    });
    match outcome {
        None | Some(Ok(_)) => Vec::new(),
        Some(Err(issue)) => {
            let mut leaves = Vec::new();
            collect(&issue, Vec::new(), &mut leaves);
            leaves.sort();
            leaves.dedup();
            leaves.retain(|(path, message)| !is_state_import_extension(method, &wire, path, message));
            leaves
                .into_iter()
                .take(8)
                .map(|(path, message)| {
                    let actual = at(&wire, &path);
                    Violation { method: method.into(), part, path: render(&path), message, actual }
                })
                .collect()
        }
    }
}

/// The one key this server adds to a reply the original defines: `stateImport` of the server config (RUST-PORT EXTENSION, see
/// `t3_contracts::state_import`). It is let through only there, and only if what it holds is a valid `StateImportStatus`.
fn is_state_import_extension(method: &str, wire: &Value, path: &[String], message: &str) -> bool {
    let steps: Vec<&str> = path.iter().map(String::as_str).collect();
    let in_config = matches!((method, steps.as_slice()), ("server.getConfig", ["value", "stateImport"]) | ("subscribeServerConfig", ["0", "config", "stateImport"]));
    if !in_config || !message.starts_with("Expected no excess property") {
        return false;
    }
    let mut found = wire;
    for step in &steps {
        found = match found {
            Value::Object(entries) => match entries.get(*step) {
                Some(next) => next,
                None => return false,
            },
            Value::Array(items) => match step.parse::<usize>().ok().and_then(|index| items.get(index)) {
                Some(next) => next,
                None => return false,
            },
            _ => return false,
        };
    }
    serde_json::from_value::<t3_contracts::StateImportStatus>(found.clone()).is_ok()
}

/// The methods the resource-telemetry service answers, which the original's group types with `EnvironmentAuthorizationError` alone
/// because its server always runs a monitor.
const RESOURCE_TELEMETRY_METHODS: [&str; 3] = ["server.getResourceTelemetryHistory", "server.retryResourceTelemetry", "subscribeResourceTelemetry"];

/// RUST-PORT EXTENSION: a server started without a resource monitor says so (`NativeServiceUnavailableError`, whose only keys are
/// `_tag` and `message`) instead of an empty history, and the diagnostics page (`rust/crates/ui/src/resource_section.rs`) shows that
/// answer. It is let through only for the resource-telemetry methods, only as the sole failure of the exit, and only in this shape.
fn is_unconfigured_service_extension(method: &str, wire: &Value) -> bool {
    if !RESOURCE_TELEMETRY_METHODS.contains(&method) || wire["_tag"] != "Failure" {
        return false;
    }
    let Some([cause]) = wire["cause"].as_array().map(Vec::as_slice) else { return false };
    let error = &cause["error"];
    cause["_tag"] == "Fail" && error["_tag"] == "NativeServiceUnavailableError" && error["message"].is_string() && error.as_object().is_some_and(|keys| keys.len() == 2)
}

/// Whether the original's group has the method, which is what a reply can be checked against.
pub fn has_schema(method: &str) -> bool {
    response_schemas().response_methods().any(|(name, _)| name == method)
}

fn collect(issue: &Issue, path: Vec<Key>, out: &mut Vec<(Vec<String>, String)>) {
    match issue {
        Issue::Leaf(message) => out.push((path.iter().map(key).collect(), message.clone())),
        Issue::Pointer(step, inner) => {
            let mut deeper = path;
            deeper.push(step.clone());
            collect(inner, deeper, out);
        }
        Issue::All(issues) => issues.iter().for_each(|inner| collect(inner, path.clone(), out)),
    }
}
fn key(step: &Key) -> String {
    match step {
        Key::Name(name) => name.clone(),
        Key::Index(index) => index.to_string(),
    }
}
fn render(path: &[String]) -> String {
    path.iter().map(|step| if step.parse::<usize>().is_ok() { format!("[{step}]") } else { format!("[{}]", Value::String(step.clone())) }).collect()
}
/// The value at the path, or what is there of the path.
fn at(wire: &Value, path: &[String]) -> String {
    let mut found = wire;
    for step in path {
        found = match found {
            Value::Object(entries) => match entries.get(step) {
                Some(next) => next,
                None => return "<missing>".into(),
            },
            Value::Array(items) => match step.parse::<usize>().ok().and_then(|index| items.get(index)) {
                Some(next) => next,
                None => return "<missing>".into(),
            },
            other => return other.to_string().chars().take(160).collect(),
        };
    }
    found.to_string().chars().take(240).collect()
}

fn append(line: Value) {
    if let Ok(file) = std::env::var("T3_OUTBOUND_VIOLATIONS_FILE") {
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(file) {
            let _ = file.write_all(format!("{line}\n").as_bytes());
        }
    }
}

/// Called by the transport for every message it writes, with the method of the request it answers.
pub fn inspect(methods: &Methods, message: &RpcServerMessage) {
    let id = match message {
        RpcServerMessage::Exit { request_id, .. } | RpcServerMessage::Chunk { request_id, .. } => request_id.clone(),
        _ => return,
    };
    let Some(method) = methods.lock().ok().and_then(|methods| methods.get(&id).cloned()) else { return };
    let part = if matches!(message, RpcServerMessage::Chunk { .. }) { "chunk" } else { "exit" };
    // A client's interrupt is answered with an exit like any other, and a failure that is a defect carries a string.
    if !has_schema(&method) {
        append(json!({"method": method, "part": part, "unschematised": true}));
        return;
    }
    let found = violations(&method, message);
    // The kinds of failure the suite has seen answered, for the coverage of the typed errors.
    let failure = match message {
        RpcServerMessage::Exit { exit: t3_contracts::RpcExit::Failure { cause }, .. } => cause.iter().find_map(|reason| match reason {
            t3_contracts::RpcCause::Fail { error } => Some(error["_tag"].clone()),
            t3_contracts::RpcCause::Die { .. } => Some(json!("<defect>")),
            _ => Some(json!("<interrupt>")),
        }),
        _ => None,
    };
    append(json!({"method": method, "part": part, "failure": failure}));
    if found.is_empty() {
        return;
    }
    report(&found);
}

/// Records the violations, and fails the test that provoked them unless `T3_OUTBOUND_CHECK=record`.
fn report(found: &[Violation]) {
    for violation in found {
        append(json!({"method": violation.method, "part": violation.part, "path": violation.path, "message": violation.message, "actual": violation.actual}));
    }
    if !found.is_empty() && std::env::var("T3_OUTBOUND_CHECK").as_deref() != Ok("record") {
        let shown: Vec<String> = found.iter().map(|v| format!("{} {}{}: {} (found {})", v.method, v.part, v.path, v.message.replace('\n', " / "), v.actual)).collect();
        panic!("the server sent a reply the original's schema does not encode:\n{}", shown.join("\n"));
    }
}

/// A thread's persisted projection, as a test reads it, checked as the document `orchestration.getThreadProjection` would send for it
/// (the replay tests compare persisted projections after normalisation, which sees none of the wire form). Each state of a thread is
/// checked once. The projection is returned as it was.
pub fn checked_thread(raw: Value) -> Value {
    use std::sync::{LazyLock, Mutex};
    static SEEN: LazyLock<Mutex<std::collections::HashSet<(String, String)>>> = LazyLock::new(Default::default);
    let state = format!("{}:{}:{}", raw["updatedAt"], raw["turnItems"].as_array().map_or(0, Vec::len), raw["visibleTurnItems"].as_array().map_or(0, Vec::len));
    let key = (raw["thread"]["id"].as_str().unwrap_or_default().to_owned(), state);
    if SEEN.lock().is_ok_and(|mut seen| seen.insert(key)) {
        let wire = json!({"_tag": "Success", "value": crate::wire_projection::projection(&raw)});
        let message = RpcServerMessage::Exit { request_id: "projection".into(), exit: serde_json::from_value(wire).expect("an exit") };
        append(json!({"method": "orchestration.getThreadProjection", "part": "exit", "failure": null, "source": "test read"}));
        report(&violations("orchestration.getThreadProjection", &message));
    }
    raw
}

/// Notes the method a request asks for, so its reply can be found a schema.
pub fn remember(methods: &Methods, id: RpcRequestId, method: &str) {
    if let Ok(mut methods) = methods.lock() {
        methods.insert(id, method.to_owned());
    }
}
