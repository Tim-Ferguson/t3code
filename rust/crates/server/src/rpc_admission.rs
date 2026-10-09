//! The original's decoding of a WebSocket RPC payload, run before the handler. Handlers decode with
//! serde into the contract types, which accept what Effect Schema refuses (a `null` for an optional
//! field, a blank id): the schemas `tools/generate_mcp_tool_schemas.mjs` writes from rpc.ts for the
//! methods in its `rpcMethods` list are interpreted here first, so such a payload is refused the way
//! the original refuses it. A method is covered by adding it to that list with cases in
//! `tools/generate_rpc_decode_cases.mjs`; `rpc_decode_tests.rs` checks each against the original.
use crate::mcp_tool_input::decoder::{Js, schemas};
use serde_json::{Value, json};

/// The wire failure for a payload the original refuses, or none when it decodes or the method is not covered.
pub fn refuse(method: &str, payload: &Value) -> Option<Value> {
    // Most methods are not covered, and their payloads are never copied.
    if !schemas().covers_rpc(method) {
        return None;
    }
    match schemas().decode_rpc(method, &Js::from_json(payload))? {
        Ok(_) => None,
        Err(issue) => Some(json!({"_tag":"SchemaDecodeError","message":issue.format()})),
    }
}
