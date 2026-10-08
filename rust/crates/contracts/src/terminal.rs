//! Terminal protocol. IDs are chosen by clients; stream cursors and raw output
//! remain separate from orchestration event cursors.
use crate::history::object_struct;
use crate::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
pub const DEFAULT_TERMINAL_ID: &str = "term-1";
pub type TerminalId = BoundedTrimmedString<128>;
pub type TerminalCols = RangeInt<1, 1000>;
pub type TerminalRows = RangeInt<1, 500>;
pub type SignedSafeInt = RangeInt<-9007199254740991, 9007199254740991>;
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct TerminalEnv(pub BTreeMap<String, BoundedString<8192>>);
impl<'de> Deserialize<'de> for TerminalEnv {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = BTreeMap::<String, serde_json::Value>::deserialize(d)?;
        let mut values = BTreeMap::new();
        // Effect Record filters unmatched keys before checking retained values
        // and the max-properties bound. They are not malformed env entries.
        for (key, value) in raw {
            let mut chars = key.chars();
            let matches = key.len() <= 128
                && chars
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
            if matches {
                values.insert(
                    key,
                    serde_json::from_value(value).map_err(serde::de::Error::custom)?,
                );
            }
        }
        if values.len() > 128 {
            return Err(serde::de::Error::custom(
                "terminal environment has at most 128 matching keys",
            ));
        }
        Ok(Self(values))
    }
}

object_struct! {pub struct TerminalThreadInput {pub thread_id:TrimmedNonEmptyString,}}
object_struct! {pub struct TerminalSessionInput {pub thread_id:TrimmedNonEmptyString,pub terminal_id:TerminalId,}}
pub type TerminalObserveInput = TerminalSessionInput;
pub type TerminalClearInput = TerminalSessionInput;
object_struct! {pub struct TerminalWriteInput {pub thread_id:TrimmedNonEmptyString,pub terminal_id:TerminalId,pub data:NonEmptyBoundedString<65536>,}}
object_struct! {pub struct TerminalResizeInput {pub thread_id:TrimmedNonEmptyString,pub terminal_id:TerminalId,pub cols:TerminalCols,pub rows:TerminalRows,}}

object_struct! {pub struct TerminalOpenInput {
    pub thread_id:TrimmedNonEmptyString, pub terminal_id:TerminalId,
    pub cwd:TrimmedNonEmptyString,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub cols:Option<Option<TerminalCols>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub rows:Option<Option<TerminalRows>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub worktree_path:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub env:Option<Option<TerminalEnv>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub provider_instance_id:Option<Option<ProviderInstanceId>>,
}}

object_struct! {pub struct TerminalAttachInput {
    pub thread_id:TrimmedNonEmptyString, pub terminal_id:TerminalId,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub cwd:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub cols:Option<Option<TerminalCols>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub rows:Option<Option<TerminalRows>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub worktree_path:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub env:Option<Option<TerminalEnv>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub provider_instance_id:Option<Option<ProviderInstanceId>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub restart_if_not_running:Option<Option<bool>>,
}}

object_struct! {pub struct TerminalRestartInput {
    pub thread_id:TrimmedNonEmptyString, pub terminal_id:TerminalId,
    pub cwd:TrimmedNonEmptyString,
    pub cols:TerminalCols,
    pub rows:TerminalRows,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub worktree_path:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub env:Option<Option<TerminalEnv>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub provider_instance_id:Option<Option<ProviderInstanceId>>,
}}
object_struct! {pub struct TerminalCloseInput {pub thread_id:TrimmedNonEmptyString,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub terminal_id:Option<Option<TerminalId>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub delete_history:Option<Option<bool>>,
}}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TerminalSessionStatus {
    Starting,
    Running,
    Exited,
    Error,
}
object_struct! {pub struct TerminalSessionSnapshot {
    pub thread_id:NonEmptyString,
    pub terminal_id:NonEmptyString,
    pub cwd:NonEmptyString,
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub worktree_path:Option<TrimmedNonEmptyString>,
    pub status:TerminalSessionStatus,
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub pid:Option<PositiveInt>,
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub exit_code:Option<SignedSafeInt>,
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub exit_signal:Option<SignedSafeInt>,
    pub history:String,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub sequence:Option<Option<NonNegativeInt>>,
    pub label:BoundedString<128>, pub updated_at:String,
}}
object_struct! {pub struct TerminalSummary {
    pub thread_id:NonEmptyString,
    pub terminal_id:NonEmptyString,
    pub cwd:NonEmptyString,
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub worktree_path:Option<TrimmedNonEmptyString>,
    pub status:TerminalSessionStatus,
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub pid:Option<PositiveInt>,
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub exit_code:Option<SignedSafeInt>,
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub exit_signal:Option<SignedSafeInt>,
    pub has_running_subprocess:bool,
    pub label:BoundedString<128>, pub updated_at:String,
}}
object_struct! {pub struct TerminalEventBase {pub thread_id:NonEmptyString,pub terminal_id:NonEmptyString,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub sequence:Option<Option<NonNegativeInt>>,
}}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum TerminalMetadataStreamEvent {
    Snapshot {
        terminals: Vec<TerminalSummary>,
    },
    Upsert {
        terminal: TerminalSummary,
    },
    Remove {
        #[serde(rename = "threadId")]
        thread_id: NonEmptyString,
        #[serde(rename = "terminalId")]
        terminal_id: NonEmptyString,
    },
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum TerminalEvent {
    Started {
        #[serde(flatten)]
        base: TerminalEventBase,
        snapshot: TerminalSessionSnapshot,
    },
    Output {
        #[serde(flatten)]
        base: TerminalEventBase,
        data: String,
    },
    Exited {
        #[serde(flatten)]
        base: TerminalEventBase,
        #[serde(
            rename = "exitCode",
            deserialize_with = "deserialize_required_nullable"
        )]
        exit_code: Option<SignedSafeInt>,
        #[serde(
            rename = "exitSignal",
            deserialize_with = "deserialize_required_nullable"
        )]
        exit_signal: Option<SignedSafeInt>,
    },
    Closed {
        #[serde(flatten)]
        base: TerminalEventBase,
    },
    Error {
        #[serde(flatten)]
        base: TerminalEventBase,
        message: NonEmptyString,
    },
    Cleared {
        #[serde(flatten)]
        base: TerminalEventBase,
    },
    Restarted {
        #[serde(flatten)]
        base: TerminalEventBase,
        snapshot: TerminalSessionSnapshot,
    },
    Activity {
        #[serde(flatten)]
        base: TerminalEventBase,
        #[serde(rename = "hasRunningSubprocess")]
        has_running_subprocess: bool,
        label: BoundedString<128>,
    },
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum TerminalAttachStreamEvent {
    Snapshot {
        snapshot: TerminalSessionSnapshot,
    },
    Output {
        #[serde(flatten)]
        base: TerminalEventBase,
        data: String,
    },
    Exited {
        #[serde(flatten)]
        base: TerminalEventBase,
        #[serde(
            rename = "exitCode",
            deserialize_with = "deserialize_required_nullable"
        )]
        exit_code: Option<SignedSafeInt>,
        #[serde(
            rename = "exitSignal",
            deserialize_with = "deserialize_required_nullable"
        )]
        exit_signal: Option<SignedSafeInt>,
    },
    Closed {
        #[serde(flatten)]
        base: TerminalEventBase,
    },
    Error {
        #[serde(flatten)]
        base: TerminalEventBase,
        message: NonEmptyString,
    },
    Cleared {
        #[serde(flatten)]
        base: TerminalEventBase,
    },
    Restarted {
        #[serde(flatten)]
        base: TerminalEventBase,
        snapshot: TerminalSessionSnapshot,
    },
    Activity {
        #[serde(flatten)]
        base: TerminalEventBase,
        #[serde(rename = "hasRunningSubprocess")]
        has_running_subprocess: bool,
        label: BoundedString<128>,
    },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TerminalHistoryOperation {
    Read,
    Truncate,
    Migrate,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalCwdNotFoundErrorTag {
    TerminalCwdNotFoundError,
}
object_struct! {pub struct TerminalCwdNotFoundError {
    #[serde(rename="_tag")]pub tag:TerminalCwdNotFoundErrorTag,
    pub cwd:String,
}}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalCwdNotDirectoryErrorTag {
    TerminalCwdNotDirectoryError,
}
object_struct! {pub struct TerminalCwdNotDirectoryError {
    #[serde(rename="_tag")]pub tag:TerminalCwdNotDirectoryErrorTag,
    pub cwd:String,
}}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalCwdStatErrorTag {
    TerminalCwdStatError,
}
object_struct! {pub struct TerminalCwdStatError {
    #[serde(rename="_tag")]pub tag:TerminalCwdStatErrorTag,
    pub cwd:String,
    pub cause:serde_json::Value,
}}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalHistoryErrorTag {
    TerminalHistoryError,
}
object_struct! {pub struct TerminalHistoryError {
    #[serde(rename="_tag")]pub tag:TerminalHistoryErrorTag,
    pub operation:TerminalHistoryOperation,
    pub thread_id:String,
    pub terminal_id:String,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub cause:Option<Option<serde_json::Value>>,
}}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalSessionLookupErrorTag {
    TerminalSessionLookupError,
}
object_struct! {pub struct TerminalSessionLookupError {
    #[serde(rename="_tag")]pub tag:TerminalSessionLookupErrorTag,
    pub thread_id:String,
    pub terminal_id:String,
}}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalProviderInstanceNotFoundErrorTag {
    TerminalProviderInstanceNotFoundError,
}
object_struct! {pub struct TerminalProviderInstanceNotFoundError {
    #[serde(rename="_tag")]pub tag:TerminalProviderInstanceNotFoundErrorTag,
    pub provider_instance_id:ProviderInstanceId,
}}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalProviderEnvironmentErrorTag {
    TerminalProviderEnvironmentError,
}
object_struct! {pub struct TerminalProviderEnvironmentError {
    #[serde(rename="_tag")]pub tag:TerminalProviderEnvironmentErrorTag,
    pub provider_instance_id:ProviderInstanceId,
    pub cause:serde_json::Value,
}}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalNotRunningErrorTag {
    TerminalNotRunningError,
}
object_struct! {pub struct TerminalNotRunningError {
    #[serde(rename="_tag")]pub tag:TerminalNotRunningErrorTag,
    pub thread_id:String,
    pub terminal_id:String,
}}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalWriteErrorTag {
    TerminalWriteError,
}
object_struct! {pub struct TerminalWriteError {
    #[serde(rename="_tag")]pub tag:TerminalWriteErrorTag,
    pub thread_id:String,
    pub terminal_id:String,
    pub terminal_pid:serde_json::Number,
    pub cause:serde_json::Value,
}}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalResizeErrorTag {
    TerminalResizeError,
}
object_struct! {pub struct TerminalResizeError {
    #[serde(rename="_tag")]pub tag:TerminalResizeErrorTag,
    pub thread_id:String,
    pub terminal_id:String,
    pub terminal_pid:serde_json::Number,
    pub cols:TerminalCols,
    pub rows:TerminalRows,
    pub cause:serde_json::Value,
}}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TerminalCwdError {
    TerminalCwdNotFoundError(TerminalCwdNotFoundError),
    TerminalCwdNotDirectoryError(TerminalCwdNotDirectoryError),
    TerminalCwdStatError(TerminalCwdStatError),
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TerminalError {
    TerminalCwdNotFoundError(TerminalCwdNotFoundError),
    TerminalCwdNotDirectoryError(TerminalCwdNotDirectoryError),
    TerminalCwdStatError(TerminalCwdStatError),
    TerminalHistoryError(TerminalHistoryError),
    TerminalSessionLookupError(TerminalSessionLookupError),
    TerminalProviderInstanceNotFoundError(TerminalProviderInstanceNotFoundError),
    TerminalProviderEnvironmentError(TerminalProviderEnvironmentError),
    TerminalNotRunningError(TerminalNotRunningError),
    TerminalWriteError(TerminalWriteError),
    TerminalResizeError(TerminalResizeError),
}
