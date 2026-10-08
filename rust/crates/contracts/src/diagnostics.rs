//! Source process diagnostics, identity-checked signals and legacy resource history.
use crate::{history::object_struct, *};
use serde::{Deserialize, Serialize};
use serde_json::Number;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerProcessSignal {
    #[serde(rename = "SIGINT")]
    Int,
    #[serde(rename = "SIGKILL")]
    Kill,
}
object_struct! {pub struct ServerProcessDiagnosticsEntry {
 pub pid:PositiveInt,pub start_time_ms:NonNegativeInt,pub ppid:NonNegativeInt,pub pgid:EffectOption<SafeInt>,pub status:TrimmedNonEmptyString,pub cpu_percent:Number,pub rss_bytes:NonNegativeInt,pub elapsed:TrimmedNonEmptyString,pub command:TrimmedNonEmptyString,pub depth:NonNegativeInt,pub child_pids:Vec<PositiveInt>,
}}
object_struct! {pub struct ServerProcessDiagnosticsError {pub message:TrimmedNonEmptyString,}}
object_struct! {pub struct ServerProcessDiagnosticsResult {
 pub server_pid:PositiveInt,pub read_at:UtcDateTime,pub process_count:NonNegativeInt,pub total_rss_bytes:NonNegativeInt,pub total_cpu_percent:Number,pub processes:Vec<ServerProcessDiagnosticsEntry>,pub error:EffectOption<ServerProcessDiagnosticsError>,
}}
object_struct! {pub struct ServerProcessResourceHistoryInput {pub window_ms:NonNegativeInt,pub bucket_ms:NonNegativeInt,}}
object_struct! {pub struct ServerProcessResourceHistoryBucket {pub started_at:UtcDateTime,pub ended_at:UtcDateTime,pub avg_cpu_percent:Number,pub max_cpu_percent:Number,pub max_rss_bytes:NonNegativeInt,pub max_process_count:NonNegativeInt,}}
object_struct! {pub struct ServerProcessResourceHistorySummary {
 pub process_key:TrimmedNonEmptyString,pub pid:PositiveInt,pub ppid:NonNegativeInt,pub command:TrimmedNonEmptyString,pub depth:NonNegativeInt,pub is_server_root:bool,pub first_seen_at:UtcDateTime,pub last_seen_at:UtcDateTime,pub current_cpu_percent:Number,pub avg_cpu_percent:Number,pub max_cpu_percent:Number,pub cpu_seconds_approx:Number,pub current_rss_bytes:NonNegativeInt,pub max_rss_bytes:NonNegativeInt,pub sample_count:NonNegativeInt,
}}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerProcessResourceHistoryFailureTag {
    #[serde(rename = "ProcessDiagnosticsQueryTimeoutError")]
    QueryTimeout,
    #[serde(rename = "ProcessDiagnosticsQueryFailedError")]
    QueryFailed,
    #[serde(rename = "ProcessDiagnosticsServerProcessSignalError")]
    ServerProcessSignal,
    #[serde(rename = "ProcessDiagnosticsNotDescendantError")]
    NotDescendant,
    #[serde(rename = "ProcessDiagnosticsSignalFailedError")]
    SignalFailed,
}
object_struct! {pub struct ServerProcessResourceHistoryFailure {pub failure_tag:ServerProcessResourceHistoryFailureTag,pub message:TrimmedNonEmptyString,}}
object_struct! {pub struct ServerProcessResourceHistoryResult {
 pub read_at:UtcDateTime,pub window_ms:NonNegativeInt,pub bucket_ms:NonNegativeInt,pub sample_interval_ms:NonNegativeInt,pub retained_sample_count:NonNegativeInt,pub total_cpu_seconds_approx:Number,pub buckets:Vec<ServerProcessResourceHistoryBucket>,pub top_processes:Vec<ServerProcessResourceHistorySummary>,pub error:EffectOption<ServerProcessResourceHistoryFailure>,
}}
object_struct! {pub struct ServerSignalProcessInput {pub pid:PositiveInt,pub start_time_ms:NonNegativeInt,pub signal:ServerProcessSignal,}}
object_struct! {pub struct ServerSignalProcessResult {pub pid:PositiveInt,pub signal:ServerProcessSignal,pub signaled:bool,pub message:EffectOption<TrimmedNonEmptyString>,}}
