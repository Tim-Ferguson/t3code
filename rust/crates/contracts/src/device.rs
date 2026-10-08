//! Device host, session, action and MCP boundaries from the original device.ts.
use crate::{history::object_struct, *};
use serde::{Deserialize, Serialize};
use serde_json::Number;
use std::collections::BTreeMap;
fn host_id(value: &str) -> Result<(), ValidationError> {
    if !value.is_empty() && value.encode_utf16().count() <= 128 {
        Ok(())
    } else {
        Err(ValidationError {
            expected: "a nonempty device host id of at most 128 UTF-16 units",
        })
    }
}
fn device_id(value: &str) -> Result<(), ValidationError> {
    if !value.is_empty() && value.encode_utf16().count() <= 256 {
        Ok(())
    } else {
        Err(ValidationError {
            expected: "a nonempty device id of at most 256 UTF-16 units",
        })
    }
}
crate::base::string_type!(DeviceHostId, host_id);
crate::base::string_type!(DeviceId, device_id);
pub const LOCAL_DEVICE_HOST_ID: &str = "local";
macro_rules! vocabulary { ($name:ident{$($variant:ident=>$value:literal),+$(,)?})=>{ #[derive(Debug,Clone,Copy,PartialEq,Eq,Serialize,Deserialize)] pub enum $name{$(#[serde(rename=$value)]$variant,)+} }; }
vocabulary! {DevicePlatform{Ios=>"ios",Android=>"android"}}
vocabulary! {DeviceHostKind{Local=>"local",Ssh=>"ssh"}}
vocabulary! {DeviceHostStatus{Disabled=>"disabled",Idle=>"idle",Installing=>"installing",Starting=>"starting",Ready=>"ready",Failed=>"failed"}}
vocabulary! {DeviceToolKind{Hub=>"hub",Agent=>"agent"}}
vocabulary! {DeviceAppearance{Light=>"light",Dark=>"dark"}}
vocabulary! {DeviceTextSize{Small=>"small",Default=>"default",Large=>"large",ExtraLarge=>"extra-large"}}
vocabulary! {DeviceColorFilter{None=>"none",Grayscale=>"grayscale",RedGreen=>"red-green",GreenRed=>"green-red",BlueYellow=>"blue-yellow"}}
vocabulary! {DeviceOrientation{Portrait=>"portrait",LandscapeLeft=>"landscape_left",PortraitUpsideDown=>"portrait_upside_down",LandscapeRight=>"landscape_right"}}
vocabulary! {DevicePermission{Camera=>"camera",Microphone=>"microphone",Photos=>"photos",Contacts=>"contacts",Calendar=>"calendar",Reminders=>"reminders",Location=>"location",Notifications=>"notifications",Motion=>"motion",MediaLibrary=>"media-library",Faceid=>"faceid"}}
vocabulary! {DeviceToggleSetting{ReduceMotion=>"reduceMotion",IncreaseContrast=>"increaseContrast",ReduceTransparency=>"reduceTransparency",ShowBorders=>"showBorders",VoiceOver=>"voiceOver",NetworkEnabled=>"networkEnabled"}}
vocabulary! {DeviceLiquidGlass{Clear=>"clear",Tinted=>"tinted"}}
vocabulary! {DevicePermissionDecision{Grant=>"grant",Revoke=>"revoke",Reset=>"reset"}}
vocabulary! {DevicePngMime{Png=>"image/png"}}
vocabulary! {DeviceBootFailureReason{DiskSpace=>"disk_space",Timeout=>"timeout",LaunchFailed=>"launch_failed"}}
vocabulary! {DeviceOperationFailureReason{CommandFailed=>"command_failed",RequestFailed=>"request_failed",InvalidPayload=>"invalid_payload",SettingsFailed=>"settings_failed",HubRejected=>"hub_rejected"}}
vocabulary! {DeviceActionFailureReason{Unsupported=>"unsupported",HelperMissing=>"helper_missing"}}
object_struct! {pub struct DeviceSummary{

pub host_id:DeviceHostId,

pub id:DeviceId,

pub platform:DevicePlatform,

pub name:TrimmedNonEmptyString,

pub version:String,

pub booted:bool,

pub physical:bool,
}}
object_struct! {pub struct DevicePlatformAvailability{

pub platform:DevicePlatform,

pub available:bool,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub reason:Option<Option<String>>,
}}
object_struct! {pub struct DeviceToolVersion{

pub required_version:String,

pub installed_versions:Vec<String>,
#[serde(deserialize_with="deserialize_required_nullable")]
pub running_version:Option<String>,
}}
object_struct! {pub struct DeviceToolVersions{

pub hub:DeviceToolVersion,

pub agent:DeviceToolVersion,
}}
object_struct! {pub struct DeviceHostSummary{

pub id:DeviceHostId,

pub kind:DeviceHostKind,

pub label:TrimmedNonEmptyString,

pub platforms:Vec<DevicePlatformAvailability>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub tools:Option<Option<DeviceToolVersions>>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub tool_inspection_error:Option<Option<String>>,

pub hub_installed:bool,

pub agent_device_installed:bool,
}}
object_struct! {pub struct DeviceSession{

pub thread_id:ThreadId,

pub host_id:DeviceHostId,

pub device_id:DeviceId,

pub platform:DevicePlatform,

pub opened_at:String,
}}
object_struct! {pub struct DeviceHostState{

pub status:DeviceHostStatus,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub detail:Option<Option<String>>,
}}
object_struct! {pub struct DeviceBootingDevice{

pub host_id:DeviceHostId,

pub id:DeviceId,

pub platform:DevicePlatform,

pub name:TrimmedNonEmptyString,

pub version:String,

pub booted:bool,

pub physical:bool,

pub thread_id:ThreadId,
}}
object_struct! {pub struct DeviceServiceState{
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub supports_host_retry:Option<Option<bool>>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub supports_tool_update:Option<Option<bool>>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub supports_tool_inspection:Option<Option<bool>>,

pub hosts:Vec<DeviceHostSummary>,

pub host_status:DeviceHostStatus,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub host_status_detail:Option<Option<String>>,

pub host_statuses:BTreeMap<DeviceHostId,DeviceHostState>,

pub devices:Vec<DeviceSummary>,

pub sessions:Vec<DeviceSession>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub booting_devices:Option<Option<Vec<DeviceBootingDevice>>>,

pub onboarding_completed:bool,

pub agent_access_enabled:bool,

pub hub_base_path:String,

pub revision:SafeInt,
}}
object_struct! {pub struct DeviceListInput{
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub update_tool:Option<Option<DeviceToolKind>>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub inspect_only:Option<Option<bool>>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub retry_host_id:Option<Option<DeviceHostId>>,
}}
object_struct! {pub struct DeviceConfigureInput{
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub enabled:Option<Option<bool>>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub agent_access_enabled:Option<Option<bool>>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub onboarding_completed:Option<Option<bool>>,
}}
object_struct! {pub struct DeviceOpenInput{

pub thread_id:ThreadId,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub host_id:Option<Option<DeviceHostId>>,

pub device_id:DeviceId,

pub platform:DevicePlatform,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub boot:Option<Option<bool>>,
}}
object_struct! {pub struct DeviceCloseInput{
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub host_id:Option<Option<DeviceHostId>>,

pub thread_id:ThreadId,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub device_id:Option<Option<DeviceId>>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub shutdown:Option<Option<bool>>,
}}
object_struct! {pub struct DeviceShutdownInput{
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub host_id:Option<Option<DeviceHostId>>,

pub device_id:DeviceId,

pub platform:DevicePlatform,
}}
object_struct! {pub struct DeviceLocation{

pub latitude:Number,

pub longitude:Number,
}}
object_struct! {pub struct DeviceSettings{
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub appearance:Option<Option<DeviceAppearance>>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub text_size:Option<Option<DeviceTextSize>>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub reduce_motion:Option<Option<bool>>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub increase_contrast:Option<Option<bool>>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub reduce_transparency:Option<Option<bool>>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub show_borders:Option<Option<bool>>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub voice_over:Option<Option<bool>>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub liquid_glass:Option<Option<DeviceLiquidGlass>>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub color_filter:Option<Option<DeviceColorFilter>>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub network_enabled:Option<Option<bool>>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub location:Option<Option<DeviceLocation>>,
}}
object_struct! {pub struct DeviceForegroundApp{

pub id:String,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub name:Option<Option<String>>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub version:Option<Option<String>>,
}}
object_struct! {pub struct DeviceDetail{

pub host_id:DeviceHostId,

pub device_id:DeviceId,

pub settings:DeviceSettings,
#[serde(deserialize_with="deserialize_required_nullable")]
pub foreground_app:Option<DeviceForegroundApp>,

pub read_at:String,
}}
object_struct! {pub struct DeviceDetailInput{
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub host_id:Option<Option<DeviceHostId>>,

pub device_id:DeviceId,
}}
object_struct! {pub struct DeviceToolOpenInput{
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub device_id:Option<Option<DeviceId>>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub platform:Option<Option<DevicePlatform>>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub host_id:Option<Option<DeviceHostId>>,
}}
object_struct! {pub struct DeviceToolTargetInput{
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub device_id:Option<Option<DeviceId>>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub host_id:Option<Option<DeviceHostId>>,
}}
object_struct! {pub struct DeviceToolCloseInput{
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub device_id:Option<Option<DeviceId>>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub host_id:Option<Option<DeviceHostId>>,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub shutdown:Option<Option<bool>>,
}}
object_struct! {pub struct DeviceToolOpenTarget{

pub host_id:DeviceHostId,

pub device_id:DeviceId,
}}
object_struct! {pub struct DeviceToolListResult{

pub host_statuses:BTreeMap<DeviceHostId,DeviceHostState>,

pub hosts:Vec<DeviceHostSummary>,

pub devices:Vec<DeviceSummary>,

pub open:Vec<DeviceToolOpenTarget>,
}}
object_struct! {pub struct DeviceAgentInvocation{

pub command:String,

pub target_args:Vec<String>,
}}
object_struct! {pub struct DeviceToolOpenResult{

pub device:DeviceSummary,

pub agent_device:DeviceAgentInvocation,

pub quick_start:String,
}}
object_struct! {pub struct DeviceScreenshotData{

pub mime_type:DevicePngMime,

pub data:String,

pub width:SafeInt,

pub height:SafeInt,
}}
object_struct! {pub struct DeviceToolScreenshotResult{

pub device:DeviceSummary,

pub screenshot:DeviceScreenshotData,
}}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DevicePushPayload {
    Text(String),
    Object(serde_json::Map<String, serde_json::Value>),
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum DeviceActionInput {
    #[serde(rename_all = "camelCase")]
    SetAppearance {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_optional"
        )]
        host_id: Option<Option<DeviceHostId>>,
        device_id: DeviceId,
        value: DeviceAppearance,
    },
    #[serde(rename_all = "camelCase")]
    SetTextSize {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_optional"
        )]
        host_id: Option<Option<DeviceHostId>>,
        device_id: DeviceId,
        value: DeviceTextSize,
    },
    #[serde(rename_all = "camelCase")]
    SetToggle {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_optional"
        )]
        host_id: Option<Option<DeviceHostId>>,
        device_id: DeviceId,
        setting: DeviceToggleSetting,
        value: bool,
    },
    #[serde(rename_all = "camelCase")]
    SetLiquidGlass {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_optional"
        )]
        host_id: Option<Option<DeviceHostId>>,
        device_id: DeviceId,
        value: DeviceLiquidGlass,
    },
    #[serde(rename_all = "camelCase")]
    SetColorFilter {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_optional"
        )]
        host_id: Option<Option<DeviceHostId>>,
        device_id: DeviceId,
        value: DeviceColorFilter,
    },
    #[serde(rename_all = "camelCase")]
    SetOrientation {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_optional"
        )]
        host_id: Option<Option<DeviceHostId>>,
        device_id: DeviceId,
        value: DeviceOrientation,
    },
    #[serde(rename_all = "camelCase")]
    SetLocation {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_optional"
        )]
        host_id: Option<Option<DeviceHostId>>,
        device_id: DeviceId,
        latitude: RangeNumber<-90, 90>,
        longitude: RangeNumber<-180, 180>,
    },
    #[serde(rename_all = "camelCase")]
    ClearLocation {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_optional"
        )]
        host_id: Option<Option<DeviceHostId>>,
        device_id: DeviceId,
    },
    #[serde(rename_all = "camelCase")]
    SetPermission {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_optional"
        )]
        host_id: Option<Option<DeviceHostId>>,
        device_id: DeviceId,
        app_id: TrimmedNonEmptyString,
        permission: DevicePermission,
        decision: DevicePermissionDecision,
    },
    #[serde(rename_all = "camelCase")]
    OpenUrl {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_optional"
        )]
        host_id: Option<Option<DeviceHostId>>,
        device_id: DeviceId,
        url: TrimmedNonEmptyString,
    },
    #[serde(rename_all = "camelCase")]
    LaunchApp {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_optional"
        )]
        host_id: Option<Option<DeviceHostId>>,
        device_id: DeviceId,
        app_id: TrimmedNonEmptyString,
    },
    #[serde(rename_all = "camelCase")]
    TerminateApp {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_optional"
        )]
        host_id: Option<Option<DeviceHostId>>,
        device_id: DeviceId,
        app_id: TrimmedNonEmptyString,
    },
    #[serde(rename_all = "camelCase")]
    Shake {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_optional"
        )]
        host_id: Option<Option<DeviceHostId>>,
        device_id: DeviceId,
    },
    #[serde(rename_all = "camelCase")]
    SendPush {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_optional"
        )]
        host_id: Option<Option<DeviceHostId>>,
        device_id: DeviceId,
        app_id: TrimmedNonEmptyString,
        payload: DevicePushPayload,
    },
}
impl DeviceActionInput {
    pub fn host_id(&self) -> Option<&DeviceHostId> {
        match self {
            Self::SetAppearance { host_id, .. }
            | Self::SetTextSize { host_id, .. }
            | Self::SetToggle { host_id, .. }
            | Self::SetLiquidGlass { host_id, .. }
            | Self::SetColorFilter { host_id, .. }
            | Self::SetOrientation { host_id, .. }
            | Self::SetLocation { host_id, .. }
            | Self::ClearLocation { host_id, .. }
            | Self::SetPermission { host_id, .. }
            | Self::OpenUrl { host_id, .. }
            | Self::LaunchApp { host_id, .. }
            | Self::TerminateApp { host_id, .. }
            | Self::Shake { host_id, .. }
            | Self::SendPush { host_id, .. } => host_id.as_ref().and_then(|host| host.as_ref()),
        }
    }
    pub fn device_id(&self) -> &DeviceId {
        match self {
            Self::SetAppearance { device_id, .. }
            | Self::SetTextSize { device_id, .. }
            | Self::SetToggle { device_id, .. }
            | Self::SetLiquidGlass { device_id, .. }
            | Self::SetColorFilter { device_id, .. }
            | Self::SetOrientation { device_id, .. }
            | Self::SetLocation { device_id, .. }
            | Self::ClearLocation { device_id, .. }
            | Self::SetPermission { device_id, .. }
            | Self::OpenUrl { device_id, .. }
            | Self::LaunchApp { device_id, .. }
            | Self::TerminateApp { device_id, .. }
            | Self::Shake { device_id, .. }
            | Self::SendPush { device_id, .. } => device_id,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeviceHostUnavailableErrorTag {
    DeviceHostUnavailableError,
}
object_struct! {pub struct DeviceHostUnavailableError {
#[serde(rename="_tag")]pub tag:DeviceHostUnavailableErrorTag,

pub host_id:DeviceHostId,

pub reason:String,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub cause:Option<Option<serde_json::Value>>,
}}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DevicePlatformUnavailableErrorTag {
    DevicePlatformUnavailableError,
}
object_struct! {pub struct DevicePlatformUnavailableError {
#[serde(rename="_tag")]pub tag:DevicePlatformUnavailableErrorTag,

pub host_id:DeviceHostId,

pub platform:DevicePlatform,

pub reason:String,
}}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeviceNotFoundErrorTag {
    DeviceNotFoundError,
}
object_struct! {pub struct DeviceNotFoundError {
#[serde(rename="_tag")]pub tag:DeviceNotFoundErrorTag,

pub host_id:DeviceHostId,

pub device_id:DeviceId,
}}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeviceBootErrorTag {
    DeviceBootError,
}
object_struct! {pub struct DeviceBootError {
#[serde(rename="_tag")]pub tag:DeviceBootErrorTag,

pub host_id:DeviceHostId,

pub device_id:DeviceId,

pub reason:DeviceBootFailureReason,

pub cause:serde_json::Value,
}}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeviceOperationErrorTag {
    DeviceOperationError,
}
object_struct! {pub struct DeviceOperationError {
#[serde(rename="_tag")]pub tag:DeviceOperationErrorTag,

pub operation:String,

pub reason:DeviceOperationFailureReason,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub exit_code:Option<Option<Number>>,

pub cause:serde_json::Value,
}}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeviceActionUnavailableErrorTag {
    DeviceActionUnavailableError,
}
object_struct! {pub struct DeviceActionUnavailableError {
#[serde(rename="_tag")]pub tag:DeviceActionUnavailableErrorTag,

pub operation:String,

pub platform:DevicePlatform,

pub reason:DeviceActionFailureReason,
}}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeviceToolUnavailableErrorTag {
    DeviceToolUnavailableError,
}
object_struct! {pub struct DeviceToolUnavailableError {
#[serde(rename="_tag")]pub tag:DeviceToolUnavailableErrorTag,

pub reason:String,
#[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
pub cause:Option<Option<serde_json::Value>>,
}}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DeviceError {
    DeviceHostUnavailableError(DeviceHostUnavailableError),
    DevicePlatformUnavailableError(DevicePlatformUnavailableError),
    DeviceNotFoundError(DeviceNotFoundError),
    DeviceBootError(DeviceBootError),
    DeviceOperationError(DeviceOperationError),
    DeviceActionUnavailableError(DeviceActionUnavailableError),
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DeviceToolError {
    DeviceHostUnavailableError(DeviceHostUnavailableError),
    DevicePlatformUnavailableError(DevicePlatformUnavailableError),
    DeviceNotFoundError(DeviceNotFoundError),
    DeviceBootError(DeviceBootError),
    DeviceOperationError(DeviceOperationError),
    DeviceActionUnavailableError(DeviceActionUnavailableError),
    DeviceToolUnavailableError(DeviceToolUnavailableError),
}
