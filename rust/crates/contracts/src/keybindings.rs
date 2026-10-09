//! `server.upsertKeybinding` and `server.removeKeybinding` payloads and results (port of
//! `ServerUpsertKeybindingInput` and friends in `packages/contracts/src/server.ts`).
use crate::history::object_struct;
use crate::*;
use serde::{Deserialize, Serialize};

// On the wire an optional property may also be null, which decodes as absent.
object_struct! {pub struct ServerKeybindingTarget {
    pub key:BoundedTrimmedString<64>,
    pub command:KeybindingCommand,
    #[serde(default,skip_serializing_if="Option::is_none")]
    pub when:Option<BoundedTrimmedString<256>>,
}}
object_struct! {pub struct ServerUpsertKeybindingInput {
    pub key:BoundedTrimmedString<64>,
    pub command:KeybindingCommand,
    #[serde(default,skip_serializing_if="Option::is_none")]
    pub when:Option<BoundedTrimmedString<256>>,
    #[serde(default,skip_serializing_if="Option::is_none")]
    pub replace:Option<ServerKeybindingTarget>,
}}
/// The remove payload is the replace target.
pub type ServerRemoveKeybindingInput = ServerKeybindingTarget;
object_struct! {pub struct ServerUpsertKeybindingResult {
    pub keybindings:ResolvedKeybindingsConfig,
    pub issues:ForwardCompatibleArray<ServerConfigIssue>,
}}
pub type ServerRemoveKeybindingResult = ServerUpsertKeybindingResult;
