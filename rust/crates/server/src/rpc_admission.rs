//! The original's decoding of a WebSocket RPC payload, run before the handler. Handlers decode with serde into the
//! contract types or read the payload by hand, which accepts what Effect Schema refuses (a blank id, a value out of
//! bounds) and refuses what it accepts (a `null` for an optional field: the JSON codec the server reads payloads with
//! decodes it as absent). The schemas `tools/generate_mcp_tool_schemas.mjs` writes from rpc.ts for every method of the RPC
//! group are interpreted here first, so a payload is refused the way the original refuses it, and a handler is given the
//! value the original's decoding makes (strings trimmed, defaults filled in, legacy aliases read, unknown keys dropped).
//! `rpc_decode_tests.rs` checks each method against the original.
use crate::mcp_tool_input::decoder::{Js, schemas};
use serde_json::{Value, json};

/// What the original's decoding makes of `payload`: the value to hand to the handler, or the wire failure (a defect, as
/// the RPC server sends it, carrying the default formatter's text). A method outside the group is not decoded.
pub fn admit(method: &str, payload: &Value) -> Result<Option<Value>, Value> {
    // RUST-PORT EXTENSION (`import_status`): no fields, so it is decoded as the original decodes a method that takes none.
    if method == t3_contracts::ACKNOWLEDGE_STATE_IMPORT_METHOD {
        return admit("projects.ensureScratch", payload);
    }
    // Methods outside the group have no schema, and their payloads are never copied.
    if !schemas().covers_rpc(method) {
        return Ok(None);
    }
    match schemas().decode_rpc(method, &Js::from_json(payload)) {
        None => Ok(None),
        Some(Ok(decoded)) => Ok(Some(decoded.to_json())),
        Some(Err(issue)) => Err(json!({"_tag":"SchemaDecodeError","message":issue.format()})),
    }
}

/// The wire failure for a payload the original refuses, or none when it decodes or the method is not covered.
pub fn refuse(method: &str, payload: &Value) -> Option<Value> {
    admit(method, payload).err()
}
