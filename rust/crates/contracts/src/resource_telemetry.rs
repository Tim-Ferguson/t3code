//! Native resource-monitor v3 wire protocol and host-power collection policy.
use crate::{history::object_struct, *};
use serde::{Deserialize, Serialize};
use serde_json::Number;
pub const RESOURCE_MONITOR_PROTOCOL_VERSION: u32 = 3;
pub type ResourceMonitorProtocolVersion = RangeInt<3, 3>;
macro_rules! vocabulary {
    ($name:ident{$($variant:ident=>$value:literal),+$(,)?})=>{
        #[derive(Debug,Clone,Copy,PartialEq,Eq,Serialize,Deserialize)]
        pub enum $name{$(#[serde(rename=$value)]$variant,)+}
    };
}
vocabulary! {BackgroundBooleanState{True=>"true",False=>"false",Unknown=>"unknown"}}
vocabulary! {HostPowerThermalState{Unknown=>"unknown",Nominal=>"nominal",Fair=>"fair",Serious=>"serious",Critical=>"critical"}}
vocabulary! {HostPowerSource{Unknown=>"unknown",NodeMacosShell=>"node-macos-shell",NodeMacosNative=>"node-macos-native",NodeLinux=>"node-linux",NodeWindows=>"node-windows",ElectronMain=>"electron-main"}}
vocabulary! {ResourceTelemetrySourceStatus{Starting=>"starting",Healthy=>"healthy",Degraded=>"degraded",Unavailable=>"unavailable",Stopped=>"stopped"}}
vocabulary! {ResourceTelemetryIoSemantics{Storage=>"storage",Logical=>"logical",AllIo=>"all-io",Unavailable=>"unavailable"}}
vocabulary! {ResourceMonitorIoSemantics{Storage=>"storage",AllIo=>"all-io"}}
vocabulary! {ResourceTelemetryProcessCategory{Server=>"server",ServerChild=>"server-child",ProviderRoot=>"provider-root",TerminalRoot=>"terminal-root",ElectronMain=>"electron-main",ElectronRenderer=>"electron-renderer",ElectronGpu=>"electron-gpu",ElectronUtility=>"electron-utility",ResourceMonitor=>"resource-monitor",UnknownT3=>"unknown-t3"}}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "_tag")]
pub enum EffectOption<T> {
    None,
    Some { value: T },
}
object_struct! {pub struct HostPowerSnapshot{
    pub source:HostPowerSource,pub idle:BackgroundBooleanState,
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub idle_seconds:Option<Number>,
    pub locked:BackgroundBooleanState,pub suspended:bool,pub on_battery:BackgroundBooleanState,
    pub low_power_mode:BackgroundBooleanState,pub thermal_state:HostPowerThermalState,pub stale:bool,pub updated_at:UtcDateTime,
}}
object_struct! {pub struct HostResourcesSnapshot{
    pub sampled_at:NonNegativeInt,
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub cpu_utilization:Option<RangeNumber<0,1>>,
    pub cpu_count:NonNegativeInt,pub available_memory_bytes:NonNegativeInt,pub total_memory_bytes:NonNegativeInt,
}}
object_struct! {pub struct ResourceTelemetryProcessIdentity{pub pid:PositiveInt,pub start_time_ms:NonNegativeInt,}}
object_struct! {pub struct ResourceMonitorExternalProcess{
    pub pid:PositiveInt,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub start_time_ms:Option<NonNegativeInt>,
}}
object_struct! {pub struct ResourceMonitorCapabilities{
    pub cumulative_cpu_time:bool,pub current_cpu_percent:bool,pub resident_memory:bool,pub virtual_memory:bool,pub io_bytes:bool,pub process_start_time:bool,pub process_tree:bool,
}}
object_struct! {pub struct ResourceMonitorProcessSample{
    pub pid:PositiveInt,pub ppid:NonNegativeInt,pub start_time_ms:NonNegativeInt,pub run_time_ms:NonNegativeInt,
    pub name:String,pub command:String,pub status:String,pub cpu_percent:Number,pub cpu_time_ms:NonNegativeInt,
    pub resident_bytes:NonNegativeInt,pub virtual_bytes:NonNegativeInt,pub io_read_bytes:NonNegativeInt,pub io_write_bytes:NonNegativeInt,pub io_semantics:ResourceMonitorIoSemantics,
}}
vocabulary! {ResourceMonitorConfigureTag{Configure=>"configure"}}
vocabulary! {ResourceMonitorSetExternalProcessesTag{SetExternalProcesses=>"setExternalProcesses"}}
vocabulary! {ResourceMonitorSetSampleIntervalTag{SetSampleInterval=>"setSampleInterval"}}
vocabulary! {ResourceMonitorSetStreamingTag{SetStreaming=>"setStreaming"}}
vocabulary! {ResourceMonitorSampleNowTag{SampleNow=>"sampleNow"}}
vocabulary! {ResourceMonitorProcessTableTag{ProcessTable=>"processTable"}}
vocabulary! {ResourceMonitorReadHistoryTag{ReadHistory=>"readHistory"}}
vocabulary! {ResourceMonitorShutdownTag{Shutdown=>"shutdown"}}
vocabulary! {ResourceMonitorHelloTag{Hello=>"hello"}}
vocabulary! {ResourceMonitorSnapshotTag{Snapshot=>"snapshot"}}
vocabulary! {ResourceMonitorHistoryChunkTag{HistoryChunk=>"historyChunk"}}
vocabulary! {ResourceMonitorErrorTag{Error=>"error"}}
object_struct! {pub struct ResourceMonitorConfigureCommand{pub version:ResourceMonitorProtocolVersion,pub r#type:ResourceMonitorConfigureTag,pub root_pid:PositiveInt,pub sample_interval_ms:NonNegativeInt,pub external_processes:Vec<ResourceMonitorExternalProcess>,}}
object_struct! {pub struct ResourceMonitorSetExternalProcessesCommand{pub version:ResourceMonitorProtocolVersion,pub r#type:ResourceMonitorSetExternalProcessesTag,pub processes:Vec<ResourceMonitorExternalProcess>,}}
object_struct! {pub struct ResourceMonitorSetSampleIntervalCommand{pub version:ResourceMonitorProtocolVersion,pub r#type:ResourceMonitorSetSampleIntervalTag,pub sample_interval_ms:NonNegativeInt,}}
object_struct! {pub struct ResourceMonitorSetStreamingCommand{pub version:ResourceMonitorProtocolVersion,pub r#type:ResourceMonitorSetStreamingTag,pub enabled:bool,}}
object_struct! {pub struct ResourceMonitorSampleNowCommand{pub version:ResourceMonitorProtocolVersion,pub r#type:ResourceMonitorSampleNowTag,pub request_id:TrimmedNonEmptyString,}}
object_struct! {pub struct ResourceMonitorProcessTableCommand{pub version:ResourceMonitorProtocolVersion,pub r#type:ResourceMonitorProcessTableTag,pub request_id:TrimmedNonEmptyString,}}
object_struct! {pub struct ResourceMonitorReadHistoryCommand{pub version:ResourceMonitorProtocolVersion,pub r#type:ResourceMonitorReadHistoryTag,pub request_id:TrimmedNonEmptyString,pub window_ms:NonNegativeInt,}}
object_struct! {pub struct ResourceMonitorShutdownCommand{pub version:ResourceMonitorProtocolVersion,pub r#type:ResourceMonitorShutdownTag,}}
object_struct! {pub struct ResourceMonitorHelloEvent{pub version:ResourceMonitorProtocolVersion,pub r#type:ResourceMonitorHelloTag,pub sidecar_version:TrimmedNonEmptyString,pub sidecar_pid:PositiveInt,pub platform:TrimmedNonEmptyString,pub arch:TrimmedNonEmptyString,pub capabilities:ResourceMonitorCapabilities,}}
object_struct! {pub struct ResourceMonitorSnapshotEvent{
    pub version:ResourceMonitorProtocolVersion,pub r#type:ResourceMonitorSnapshotTag,pub sequence:NonNegativeInt,pub sampled_at_unix_ms:NonNegativeInt,pub collection_duration_micros:NonNegativeInt,pub scanned_process_count:NonNegativeInt,pub retained_process_count:NonNegativeInt,pub inaccessible_process_count:NonNegativeInt,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub request_id:Option<TrimmedNonEmptyString>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub external_processes:Option<Vec<ResourceMonitorExternalProcess>>,
    pub processes:Vec<ResourceMonitorProcessSample>,
}}
object_struct! {pub struct ResourceMonitorProcessTableEntry{pub pid:PositiveInt,pub ppid:NonNegativeInt,pub name:String,}}
object_struct! {pub struct ResourceMonitorProcessTableEvent{pub version:ResourceMonitorProtocolVersion,pub r#type:ResourceMonitorProcessTableTag,pub request_id:TrimmedNonEmptyString,pub processes:Vec<ResourceMonitorProcessTableEntry>,}}
object_struct! {pub struct ResourceMonitorHistoryChunkEvent{pub version:ResourceMonitorProtocolVersion,pub r#type:ResourceMonitorHistoryChunkTag,pub request_id:TrimmedNonEmptyString,pub done:bool,pub snapshots:Vec<ResourceMonitorSnapshotEvent>,}}
object_struct! {pub struct ResourceMonitorErrorEvent{pub version:ResourceMonitorProtocolVersion,pub r#type:ResourceMonitorErrorTag,pub code:TrimmedNonEmptyString,pub message:TrimmedNonEmptyString,pub recoverable:bool,}}
macro_rules! protocol_union{
    ($name:ident{$($variant:ident($payload:ty)=>$tag:literal),+$(,)?})=>{
        #[derive(Debug,Clone,PartialEq,Serialize)]#[serde(untagged)]
        pub enum $name{$($variant($payload),)+}
        impl<'de> Deserialize<'de> for $name{
            fn deserialize<D:serde::Deserializer<'de>>(d:D)->Result<Self,D::Error>{
                let value=serde_json::Value::deserialize(d)?;
                match value.get("type").and_then(serde_json::Value::as_str){
                    $(Some($tag)=>serde_json::from_value(value).map(Self::$variant).map_err(serde::de::Error::custom),)+
                    _=>Err(serde::de::Error::custom("unknown resource protocol message type")),
                }
            }
        }
    };
}
protocol_union! {ResourceMonitorCommand{
    Configure(ResourceMonitorConfigureCommand)=>"configure",
    SetExternalProcesses(ResourceMonitorSetExternalProcessesCommand)=>"setExternalProcesses",
    SetSampleInterval(ResourceMonitorSetSampleIntervalCommand)=>"setSampleInterval",
    SetStreaming(ResourceMonitorSetStreamingCommand)=>"setStreaming",
    SampleNow(ResourceMonitorSampleNowCommand)=>"sampleNow",
    ProcessTable(ResourceMonitorProcessTableCommand)=>"processTable",
    ReadHistory(ResourceMonitorReadHistoryCommand)=>"readHistory",
    Shutdown(ResourceMonitorShutdownCommand)=>"shutdown",
}}
protocol_union! {ResourceMonitorEvent{
    Hello(ResourceMonitorHelloEvent)=>"hello",Snapshot(ResourceMonitorSnapshotEvent)=>"snapshot",
    ProcessTable(ResourceMonitorProcessTableEvent)=>"processTable",HistoryChunk(ResourceMonitorHistoryChunkEvent)=>"historyChunk",Error(ResourceMonitorErrorEvent)=>"error",
}}
vocabulary! {DesktopElectronProcessType{Browser=>"Browser",Tab=>"Tab",Utility=>"Utility",Zygote=>"Zygote",SandboxHelper=>"Sandbox helper",Gpu=>"GPU",PepperPlugin=>"Pepper Plugin",PepperPluginBroker=>"Pepper Plugin Broker",Unknown=>"Unknown"}}
object_struct! {pub struct DesktopElectronProcessMetric{
    pub pid:PositiveInt,pub creation_time_ms:NonNegativeInt,pub r#type:DesktopElectronProcessType,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub name:Option<String>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub service_name:Option<String>,
    pub cpu_percent:Number,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub cumulative_cpu_seconds:Option<Number>,
    pub idle_wakeups_per_second:Number,pub working_set_bytes:NonNegativeInt,pub peak_working_set_bytes:NonNegativeInt,
}}
vocabulary! {DesktopHostTelemetrySnapshotTag{Snapshot=>"desktopTelemetry"}}
vocabulary! {DesktopHostTelemetryHelloTag{Hello=>"desktopTelemetryHello"}}
object_struct! {pub struct DesktopHostTelemetrySnapshot{
    pub version:RangeInt<1,1>,pub r#type:DesktopHostTelemetrySnapshotTag,pub sequence:NonNegativeInt,pub sampled_at_unix_ms:NonNegativeInt,pub electron_pid:PositiveInt,pub power:HostPowerSnapshot,
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub speed_limit_percent:Option<Number>,
    pub electron_processes:Vec<DesktopElectronProcessMetric>,
}}
object_struct! {pub struct DesktopHostTelemetryHello{pub version:RangeInt<1,1>,pub r#type:DesktopHostTelemetryHelloTag,pub electron_pid:PositiveInt,}}
object_struct! {pub struct ResourceTelemetryProcess{
    pub identity:ResourceTelemetryProcessIdentity,pub ppid:NonNegativeInt,pub child_pids:Vec<PositiveInt>,pub depth:NonNegativeInt,pub name:String,pub command:String,pub status:String,pub category:ResourceTelemetryProcessCategory,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub electron_type:Option<DesktopElectronProcessType>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub electron_service_name:Option<String>,
    pub cpu_percent:Number,pub cpu_time_ms:NonNegativeInt,pub resident_bytes:NonNegativeInt,pub peak_resident_bytes:NonNegativeInt,pub virtual_bytes:NonNegativeInt,pub io_read_bytes:NonNegativeInt,pub io_write_bytes:NonNegativeInt,pub io_read_bytes_per_second:Number,pub io_write_bytes_per_second:Number,pub io_semantics:ResourceTelemetryIoSemantics,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub idle_wakeups_per_second:Option<Number>,
    pub run_time_ms:NonNegativeInt,pub first_seen_at:UtcDateTime,pub last_seen_at:UtcDateTime,
}}
object_struct! {pub struct ResourceTelemetryAggregate{
    pub process_count:NonNegativeInt,pub current_cpu_percent:Number,pub cpu_time_ms:NonNegativeInt,pub current_rss_bytes:NonNegativeInt,pub peak_rss_bytes:NonNegativeInt,pub io_read_bytes:NonNegativeInt,pub io_write_bytes:NonNegativeInt,pub io_read_bytes_per_second:Number,pub io_write_bytes_per_second:Number,pub process_starts:NonNegativeInt,pub process_exits:NonNegativeInt,
}}
object_struct! {pub struct ResourceTelemetryGroups{pub backend:ResourceTelemetryAggregate,pub electron:ResourceTelemetryAggregate,pub monitor:ResourceTelemetryAggregate,pub all_t3:ResourceTelemetryAggregate,}}
object_struct! {pub struct ResourceTelemetrySourceHealth{pub status:ResourceTelemetrySourceStatus,pub last_sample_at:EffectOption<UtcDateTime>,pub last_error:EffectOption<TrimmedNonEmptyString>,}}
object_struct! {pub struct ResourceTelemetryHealth{
    pub native:ResourceTelemetrySourceHealth,pub desktop:ResourceTelemetrySourceHealth,pub sidecar_version:EffectOption<TrimmedNonEmptyString>,pub sidecar_pid:EffectOption<PositiveInt>,pub restart_count:NonNegativeInt,pub collection_duration_micros:NonNegativeInt,pub scanned_process_count:NonNegativeInt,pub retained_process_count:NonNegativeInt,pub inaccessible_process_count:NonNegativeInt,
}}
object_struct! {pub struct ResourceAttributionEntry{pub component:TrimmedNonEmptyString,pub operation:TrimmedNonEmptyString,pub logical_read_bytes:NonNegativeInt,pub logical_write_bytes:NonNegativeInt,pub count:NonNegativeInt,pub duration_ms:NonNegativeInt,}}
object_struct! {pub struct ResourceAttributionSnapshot{pub read_at:UtcDateTime,pub entries:Vec<ResourceAttributionEntry>,}}
object_struct! {pub struct ResourceTelemetrySnapshot{
    pub read_at:UtcDateTime,pub sample_interval_ms:NonNegativeInt,pub processes:Vec<ResourceTelemetryProcess>,pub groups:ResourceTelemetryGroups,pub power:HostPowerSnapshot,pub speed_limit_percent:EffectOption<Number>,pub attribution:ResourceAttributionSnapshot,pub health:ResourceTelemetryHealth,
}}
object_struct! {pub struct ResourceTelemetryHistoryInput{pub window_ms:NonNegativeInt,pub bucket_ms:NonNegativeInt,}}
object_struct! {pub struct ResourceTelemetryHistoryBucket{pub started_at:UtcDateTime,pub ended_at:UtcDateTime,pub avg_cpu_percent:Number,pub max_cpu_percent:Number,pub max_rss_bytes:NonNegativeInt,pub io_read_bytes:NonNegativeInt,pub io_write_bytes:NonNegativeInt,pub max_process_count:NonNegativeInt,}}
object_struct! {pub struct ResourceTelemetryProcessSummary{
    pub identity:ResourceTelemetryProcessIdentity,pub ppid:NonNegativeInt,pub depth:NonNegativeInt,pub name:String,pub command:String,pub category:ResourceTelemetryProcessCategory,pub first_seen_at:UtcDateTime,pub last_seen_at:UtcDateTime,pub current_cpu_percent:Number,pub avg_cpu_percent:Number,pub max_cpu_percent:Number,pub cpu_time_ms:NonNegativeInt,pub current_rss_bytes:NonNegativeInt,pub peak_rss_bytes:NonNegativeInt,pub io_read_bytes:NonNegativeInt,pub io_write_bytes:NonNegativeInt,pub io_semantics:ResourceTelemetryIoSemantics,pub sample_count:NonNegativeInt,
}}
object_struct! {pub struct ResourceTelemetryHistory{pub read_at:UtcDateTime,pub window_ms:NonNegativeInt,pub bucket_ms:NonNegativeInt,pub sample_interval_ms:NonNegativeInt,pub retained_sample_count:NonNegativeInt,pub buckets:Vec<ResourceTelemetryHistoryBucket>,pub top_processes:Vec<ResourceTelemetryProcessSummary>,pub health:ResourceTelemetryHealth,}}
object_struct! {pub struct ResourceTelemetryRetryResult{pub accepted:bool,pub snapshot:ResourceTelemetrySnapshot,}}
