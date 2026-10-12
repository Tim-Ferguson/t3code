//! Preview automation (packages/contracts/src/previewAutomation.ts): what an agent's browser tools ask
//! of a browser host, what a host answers, and the failures between them. The tools' own input schemas
//! are interpreted from the original's catalog (`mcp_tool_schemas.json`), so a call reaches a host as
//! the value they decode it to and only the shapes the server itself reads or writes are typed here.
use crate::preview_session::{Defect, literal, tagged_union, vocabulary};
use crate::{
    EnvironmentId, NonNegativeInt, PreviewRenderedViewportSize, PreviewTabId, PreviewViewportSetting, ProviderInstanceId, SafeInt, ThreadId, TrimmedNonEmptyString,
    ValidationError, deserialize_optional, deserialize_required_nullable, history::object_struct, string_type,
};
use serde::{Deserialize, Serialize};
use serde_json::{Number, Value};

/// Operations understood by desktop hosts predating viewport resizing.
pub const PREVIEW_AUTOMATION_V1_OPERATIONS: [PreviewAutomationOperation; 12] = {
    use PreviewAutomationOperation::*;
    [Status, Open, Navigate, Snapshot, Click, Type, Press, Scroll, Evaluate, WaitFor, RecordingStart, RecordingStop]
};
/// Every operation the server's own browser understands.
pub const PREVIEW_AUTOMATION_SERVER_OPERATIONS: [PreviewAutomationOperation; 20] = {
    use PreviewAutomationOperation::*;
    [
        Status, Open, Navigate, Snapshot, Click, Type, Press, Scroll, Evaluate, WaitFor, RecordingStart, RecordingStop, Resize, SetColorScheme, Dialog, Close, Upload,
        Hover, Select, Drag,
    ]
};
pub const PREVIEW_RECORDING_STOP_TIMEOUT_MS: u64 = 120_000;
/// The automation host id of an environment's own server browser.
pub const SERVER_BROWSER_AUTOMATION_CLIENT_ID: &str = "server-browser";

vocabulary!(PreviewAutomationOperation {
    Status => "status", Open => "open", Navigate => "navigate", Snapshot => "snapshot", Click => "click", Type => "type", Press => "press", Scroll => "scroll",
    Evaluate => "evaluate", WaitFor => "waitFor", RecordingStart => "recordingStart", RecordingStop => "recordingStop", Resize => "resize",
    SetColorScheme => "setColorScheme", Dialog => "dialog", Close => "close", Upload => "upload", Hover => "hover", Select => "select", Drag => "drag",
});
vocabulary!(PreviewAutomationColorScheme { System => "system", Light => "light", Dark => "dark" });
vocabulary!(PreviewAutomationControlOwner { Agent => "agent", Human => "human", Unclaimed => "unclaimed" });
vocabulary!(PreviewAutomationControlReason {
    AgentMismatch => "agentMismatch", HumanControl => "humanControl", TabRequired => "tabRequired", Closed => "closed", Interrupted => "interrupted",
    DialogPending => "dialogPending", TabLimit => "tabLimit",
});
vocabulary!(PreviewAutomationDetailKind { Null => "null", Array => "array", Object => "object", String => "string", Number => "number", Boolean => "boolean" });
vocabulary!(PreviewAutomationActionStatus { Running => "running", Succeeded => "succeeded", Failed => "failed", Interrupted => "interrupted" });
vocabulary!(PreviewAutomationSelectorKind { Locator => "locator", Selector => "selector" });
vocabulary!(PreviewAutomationEditableTarget { FocusedElement => "focused-element", Locator => "locator", Selector => "selector" });
vocabulary!(PreviewUrlResolutionKind { Direct => "direct", DirectPrivateNetwork => "direct-private-network" });

fn client_id(value: &str) -> Result<(), ValidationError> {
    if !value.is_empty() && value.encode_utf16().count() <= 128 {
        Ok(())
    } else {
        Err(ValidationError { expected: "a nonempty automation client id of at most 128 UTF-16 units" })
    }
}
fn connection_id(value: &str) -> Result<(), ValidationError> {
    if !value.is_empty() && value.encode_utf16().count() <= 64 {
        Ok(())
    } else {
        Err(ValidationError { expected: "a nonempty automation connection id of at most 64 UTF-16 units" })
    }
}
string_type!(PreviewAutomationClientId, client_id);
string_type!(PreviewAutomationConnectionId, connection_id);

object_struct! {pub struct PreviewAutomationTabTargetInput {
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub tab_id:Option<Option<PreviewTabId>>,
}}

object_struct! {pub struct PreviewAutomationControl {pub owner:PreviewAutomationControlOwner,pub owned_by_caller:bool,pub generation:Number,}}
object_struct! {pub struct PreviewAutomationDialog {pub r#type:String,pub message:String,pub default_value:String,}}
object_struct! {pub struct PreviewAutomationFileChooser {pub multiple:bool,pub accept:String,}}
object_struct! {pub struct PreviewAutomationDownload {pub file_name:String,pub path:String,pub size_bytes:Number,pub url:String,pub completed_at:String,}}
object_struct! {pub struct PreviewAutomationTab {
    pub tab_id:PreviewTabId,
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub url:Option<String>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub opener_tab_id:Option<Option<PreviewTabId>>,
}}
object_struct! {pub struct PreviewAutomationStatus {
    pub available:bool,pub visible:bool,
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub tab_id:Option<PreviewTabId>,
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub url:Option<String>,
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub title:Option<String>,
    pub loading:bool,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub control:Option<Option<PreviewAutomationControl>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub dialog:Option<Option<PreviewAutomationDialog>>,
    /// Absent from desktop hosts predating viewport sizing.
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub viewport_setting:Option<Option<PreviewViewportSetting>>,
    /// The measured guest-page viewport in CSS pixels when a page is ready.
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub viewport:Option<Option<PreviewRenderedViewportSize>>,
    /// Server hosts: a file picker the page opened, answered with `preview_upload`.
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub file_chooser:Option<Option<PreviewAutomationFileChooser>>,
    /// Server hosts: files this tab downloaded, kept on the environment until the tab closes.
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub downloads:Option<Option<Vec<PreviewAutomationDownload>>>,
    /// Server hosts: every tab this agent session owns, including popups its pages opened.
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub tabs:Option<Option<Vec<PreviewAutomationTab>>>,
}}
object_struct! {pub struct PreviewAutomationResizeResult {pub tab_id:PreviewTabId,pub setting:PreviewViewportSetting,pub viewport:PreviewRenderedViewportSize,}}
object_struct! {pub struct PreviewAutomationSetColorSchemeResult {pub tab_id:PreviewTabId,pub color_scheme:PreviewAutomationColorScheme,}}
object_struct! {pub struct PreviewAutomationSelectResult {pub selected:Vec<String>,}}
object_struct! {pub struct PreviewAutomationElement {
    pub tag:String,
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub role:Option<String>,
    pub name:String,pub selector:String,pub x:Number,pub y:Number,pub width:Number,pub height:Number,
}}
object_struct! {pub struct PreviewAutomationConsoleEntry {
    pub level:String,pub text:String,pub timestamp:String,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub source:Option<Option<String>>,
}}
object_struct! {pub struct PreviewAutomationNetworkEntry {
    pub url:String,pub method:String,
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub status:Option<Number>,
    pub failed:bool,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub error_text:Option<Option<String>>,
    pub timestamp:String,
}}
object_struct! {pub struct PreviewAutomationActionEvent {
    pub id:String,pub action:String,pub status:PreviewAutomationActionStatus,pub started_at:String,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub completed_at:Option<Option<String>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub error:Option<Option<String>>,
}}
literal!(PreviewPngMime = "image/png");
object_struct! {pub struct PreviewAutomationScreenshot {pub mime_type:PreviewPngMime,pub data:String,pub width:SafeInt,pub height:SafeInt,}}
object_struct! {pub struct PreviewAutomationSnapshot {
    pub url:String,pub title:String,pub loading:bool,pub visible_text:String,
    pub interactive_elements:Vec<PreviewAutomationElement>,
    pub accessibility_tree:Value,
    pub console_entries:Vec<PreviewAutomationConsoleEntry>,
    pub network_entries:Vec<PreviewAutomationNetworkEntry>,
    pub action_timeline:Vec<PreviewAutomationActionEvent>,
    pub screenshot:PreviewAutomationScreenshot,
}}
object_struct! {pub struct PreviewAutomationRecordingStatus {
    pub tab_id:PreviewTabId,pub recording:bool,
    #[serde(deserialize_with="deserialize_required_nullable")]
    pub started_at:Option<String>,
}}
object_struct! {pub struct PreviewAutomationRecordingArtifact {
    pub id:String,pub tab_id:PreviewTabId,pub path:String,pub mime_type:String,pub size_bytes:SafeInt,pub created_at:String,
}}

object_struct! {pub struct PreviewAutomationHostIdentity {pub client_id:PreviewAutomationClientId,pub environment_id:EnvironmentId,}}
object_struct! {pub struct PreviewAutomationHost {
    pub client_id:PreviewAutomationClientId,pub environment_id:EnvironmentId,
    /// Absent means the operation set that predates capability negotiation.
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub supported_operations:Option<Option<Vec<PreviewAutomationOperation>>>,
}}
object_struct! {pub struct PreviewAutomationLiveTab {
    pub thread_id:ThreadId,pub tab_id:PreviewTabId,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub visible:Option<Option<bool>>,
}}
object_struct! {pub struct PreviewAutomationHostFocus {
    pub client_id:PreviewAutomationClientId,pub environment_id:EnvironmentId,pub connection_id:PreviewAutomationConnectionId,pub focused:bool,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub live_tabs:Option<Option<Vec<PreviewAutomationLiveTab>>>,
}}
object_struct! {pub struct PreviewAutomationRequest {
    pub request_id:TrimmedNonEmptyString,pub thread_id:ThreadId,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub tab_id:Option<Option<PreviewTabId>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub tab_id_explicit:Option<Option<bool>>,
    /// Supplied by the broker from the authenticated scope, never from tool input.
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub agent_session_id:Option<Option<String>>,
    pub operation:PreviewAutomationOperation,
    pub input:Value,
    pub timeout_ms:crate::RangeInt<1,9_007_199_254_740_991>,
}}
literal!(PreviewAutomationConnectedType = "connected");
literal!(PreviewAutomationRequestType = "request");
object_struct! {pub struct PreviewAutomationConnectedEvent {pub r#type:PreviewAutomationConnectedType,pub connection_id:PreviewAutomationConnectionId,}}
object_struct! {pub struct PreviewAutomationRequestEvent {
    pub r#type:PreviewAutomationRequestType,pub connection_id:PreviewAutomationConnectionId,pub request:PreviewAutomationRequest,
}}
tagged_union! {PreviewAutomationStreamEvent by "type" {
    Connected(PreviewAutomationConnectedEvent)=>"connected",Request(PreviewAutomationRequestEvent)=>"request",
}}
object_struct! {pub struct PreviewAutomationRemoteError {
    #[serde(rename="_tag")]pub tag:TrimmedNonEmptyString,pub message:String,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub detail:Option<Option<Value>>,
}}
object_struct! {pub struct PreviewAutomationResponse {
    pub client_id:PreviewAutomationClientId,pub connection_id:PreviewAutomationConnectionId,pub request_id:TrimmedNonEmptyString,pub ok:bool,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub result:Option<Option<Value>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub error:Option<Option<PreviewAutomationRemoteError>>,
}}
object_struct! {pub struct PreviewUrlResolution {
    pub requested_url:String,pub resolved_url:String,pub resolution_kind:PreviewUrlResolutionKind,pub environment_id:EnvironmentId,
}}

// Failures. Each is the tagged struct the original encodes; `message` is the text its class computes, which an agent reads.

macro_rules! automation_error {
    ($(#[$meta:meta])* $name:ident, $tag:ident = $literal:literal { $($fields:tt)* }) => {
        literal!($tag = $literal);
        object_struct! {$(#[$meta])* pub struct $name { #[serde(rename="_tag")] pub tag:$tag, $($fields)* }}
    };
}
/// The fields every failure carries that names the request it came from; `remote` adds what the host said about its own failure
/// (kept for diagnosis, never shown to the agent as text).
macro_rules! request_error {
    ($name:ident, $tag:ident = $literal:literal, plain { $($fields:tt)* }) => {
        automation_error! {$name, $tag = $literal {
            pub operation:PreviewAutomationOperation,pub environment_id:EnvironmentId,pub thread_id:ThreadId,
            pub provider_session_id:TrimmedNonEmptyString,pub provider_instance_id:ProviderInstanceId,
            pub client_id:TrimmedNonEmptyString,pub connection_id:PreviewAutomationConnectionId,pub request_id:TrimmedNonEmptyString,
            #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
            pub tab_id:Option<Option<PreviewTabId>>,
            pub timeout_ms:crate::RangeInt<1,9_007_199_254_740_991>,
            $($fields)*
        }}
    };
    ($name:ident, $tag:ident = $literal:literal, remote { $($fields:tt)* }) => {
        request_error! {$name, $tag = $literal, plain {
            pub remote_tag:TrimmedNonEmptyString,pub remote_message_length:NonNegativeInt,
            #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
            pub remote_detail_kind:Option<Option<PreviewAutomationDetailKind>>,
            pub cause:Defect,
            $($fields)*
        }}
    };
}

automation_error! {
/// The credential does not grant the preview capability. Thread fields are absent when the caller signed in from outside a thread.
PreviewAutomationUnavailableError, PreviewAutomationUnavailableErrorTag = "PreviewAutomationUnavailableError" {
    pub capability:PreviewCapabilityLiteral,pub environment_id:EnvironmentId,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub thread_id:Option<Option<ThreadId>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub provider_session_id:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub provider_instance_id:Option<Option<ProviderInstanceId>>,
}}
literal!(PreviewCapabilityLiteral = "preview");
automation_error! {PreviewAutomationNoAvailableHostError, PreviewAutomationNoAvailableHostErrorTag = "PreviewAutomationNoAvailableHostError" {
    pub operation:PreviewAutomationOperation,pub environment_id:EnvironmentId,pub thread_id:ThreadId,
    pub provider_session_id:TrimmedNonEmptyString,pub provider_instance_id:ProviderInstanceId,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub client_id:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub connection_id:Option<Option<PreviewAutomationConnectionId>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub request_id:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub tab_id:Option<Option<PreviewTabId>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub timeout_ms:Option<Option<crate::RangeInt<1,9_007_199_254_740_991>>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub remote_tag:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub remote_message_length:Option<Option<NonNegativeInt>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub remote_detail_kind:Option<Option<PreviewAutomationDetailKind>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub cause:Option<Option<Defect>>,
}}
request_error! {PreviewAutomationUnsupportedClientError, PreviewAutomationUnsupportedClientErrorTag = "PreviewAutomationUnsupportedClientError", remote {} }
request_error! {PreviewAutomationTabNotFoundError, PreviewAutomationTabNotFoundErrorTag = "PreviewAutomationTabNotFoundError", remote {} }
request_error! {PreviewAutomationTimeoutError, PreviewAutomationTimeoutErrorTag = "PreviewAutomationTimeoutError", plain {
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub remote_tag:Option<Option<TrimmedNonEmptyString>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub remote_message_length:Option<Option<NonNegativeInt>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub remote_detail_kind:Option<Option<PreviewAutomationDetailKind>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub cause:Option<Option<Defect>>,
}}
request_error! {PreviewAutomationControlInterruptedError, PreviewAutomationControlInterruptedErrorTag = "PreviewAutomationControlInterruptedError", remote {
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub reason:Option<Option<PreviewAutomationControlReason>>,
}}
request_error! {PreviewAutomationExecutionError, PreviewAutomationExecutionErrorTag = "PreviewAutomationExecutionError", remote {
    /// What the server's own browser reported, such as a page that refused the connection. Absent for other hosts.
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub reason:Option<Option<String>>,
}}
request_error! {PreviewAutomationInvalidSelectorError, PreviewAutomationInvalidSelectorErrorTag = "PreviewAutomationInvalidSelectorError", remote {
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub selector_kind:Option<Option<PreviewAutomationSelectorKind>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub selector_length:Option<Option<NonNegativeInt>>,
    /// The selector was an `aria-ref` from an older snapshot.
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub stale_ref:Option<Option<bool>>,
}}
request_error! {PreviewAutomationTargetNotEditableError, PreviewAutomationTargetNotEditableErrorTag = "PreviewAutomationTargetNotEditableError", remote {
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub selector_kind:Option<Option<PreviewAutomationEditableTarget>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub selector_length:Option<Option<NonNegativeInt>>,
}}
request_error! {PreviewAutomationResultTooLargeError, PreviewAutomationResultTooLargeErrorTag = "PreviewAutomationResultTooLargeError", remote {
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub maximum_bytes:Option<Option<crate::PositiveInt>>,
}}
request_error! {PreviewAutomationClientDisconnectedError, PreviewAutomationClientDisconnectedErrorTag = "PreviewAutomationClientDisconnectedError", plain {} }
request_error! {PreviewAutomationRequestQueueClosedError, PreviewAutomationRequestQueueClosedErrorTag = "PreviewAutomationRequestQueueClosedError", plain {} }
request_error! {PreviewAutomationRemoteUnavailableError, PreviewAutomationRemoteUnavailableErrorTag = "PreviewAutomationRemoteUnavailableError", remote {} }
request_error! {PreviewAutomationMalformedResponseError, PreviewAutomationMalformedResponseErrorTag = "PreviewAutomationMalformedResponseError", plain {} }

macro_rules! recording_error {
    ($name:ident, $tag:ident = $literal:literal) => {
        automation_error! {$name, $tag = $literal {
            pub thread_id:ThreadId,
            #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
            pub cause:Option<Option<Defect>>,
        }}
    };
}
recording_error!(PreviewAutomationRecordingTransferError, PreviewAutomationRecordingTransferErrorTag = "PreviewAutomationRecordingTransferError");
recording_error!(
    PreviewAutomationRecordingDesktopUpdateRequiredError,
    PreviewAutomationRecordingDesktopUpdateRequiredErrorTag = "PreviewAutomationRecordingDesktopUpdateRequiredError"
);
recording_error!(PreviewAutomationRecordingTooLargeError, PreviewAutomationRecordingTooLargeErrorTag = "PreviewAutomationRecordingTooLargeError");
recording_error!(
    PreviewAutomationRecordingDeadlineExpiredError,
    PreviewAutomationRecordingDeadlineExpiredErrorTag = "PreviewAutomationRecordingDeadlineExpiredError"
);

tagged_union! {PreviewAutomationError by "_tag" {
    RecordingTransfer(PreviewAutomationRecordingTransferError)=>"PreviewAutomationRecordingTransferError",
    RecordingDesktopUpdateRequired(PreviewAutomationRecordingDesktopUpdateRequiredError)=>"PreviewAutomationRecordingDesktopUpdateRequiredError",
    RecordingTooLarge(PreviewAutomationRecordingTooLargeError)=>"PreviewAutomationRecordingTooLargeError",
    RecordingDeadlineExpired(PreviewAutomationRecordingDeadlineExpiredError)=>"PreviewAutomationRecordingDeadlineExpiredError",
    Unavailable(PreviewAutomationUnavailableError)=>"PreviewAutomationUnavailableError",
    NoAvailableHost(PreviewAutomationNoAvailableHostError)=>"PreviewAutomationNoAvailableHostError",
    UnsupportedClient(PreviewAutomationUnsupportedClientError)=>"PreviewAutomationUnsupportedClientError",
    TabNotFound(PreviewAutomationTabNotFoundError)=>"PreviewAutomationTabNotFoundError",
    Timeout(PreviewAutomationTimeoutError)=>"PreviewAutomationTimeoutError",
    ControlInterrupted(PreviewAutomationControlInterruptedError)=>"PreviewAutomationControlInterruptedError",
    Execution(PreviewAutomationExecutionError)=>"PreviewAutomationExecutionError",
    InvalidSelector(PreviewAutomationInvalidSelectorError)=>"PreviewAutomationInvalidSelectorError",
    TargetNotEditable(PreviewAutomationTargetNotEditableError)=>"PreviewAutomationTargetNotEditableError",
    ResultTooLarge(PreviewAutomationResultTooLargeError)=>"PreviewAutomationResultTooLargeError",
    ClientDisconnected(PreviewAutomationClientDisconnectedError)=>"PreviewAutomationClientDisconnectedError",
    RequestQueueClosed(PreviewAutomationRequestQueueClosedError)=>"PreviewAutomationRequestQueueClosedError",
    RemoteUnavailable(PreviewAutomationRemoteUnavailableError)=>"PreviewAutomationRemoteUnavailableError",
    MalformedResponse(PreviewAutomationMalformedResponseError)=>"PreviewAutomationMalformedResponseError",
}}

fn operation_name(operation: PreviewAutomationOperation) -> &'static str {
    operation.as_str()
}

impl PreviewAutomationError {
    /// The text the original's class computes for the failure, which is what an agent reads.
    pub fn message(&self) -> String {
        use crate::preview_session::Present;
        use PreviewAutomationError::*;
        match self {
            RecordingTransfer(_) => "Preview recording could not be saved to the agent environment. The saved copy remains on the desktop.".into(),
            RecordingDesktopUpdateRequired(_) => "Update the desktop app to transfer recordings. The recording remains on the desktop.".into(),
            RecordingTooLarge(_) => "The recording exceeds 50 MiB. The saved copy remains on the desktop.".into(),
            RecordingDeadlineExpired(_) => "The recording transfer deadline expired. The saved copy remains on the desktop.".into(),
            Unavailable(_) => "MCP credential does not grant the preview capability: browser preview tools are off for this thread. Do not retry them. To check a page, use a headless browser from the shell, such as Playwright, or curl. The user can turn on \"Agent browser access\" in Settings; it applies when the agent session next starts.".into(),
            NoAvailableHost(error) => format!("No preview automation host is available for {} in environment {}. Preview tools run in a T3 Code desktop app that is open and connected to this environment; a headless server has no browser of its own. Do not retry. To check a page, use a headless browser from the shell, such as Playwright, or curl, or ask the user to open this thread in the T3 Code desktop app.", operation_name(error.operation), error.environment_id),
            UnsupportedClient(error) => format!("Preview automation client {} does not support {}.", error.client_id, operation_name(error.operation)),
            TabNotFound(error) => match error.tab_id.present() {
                Some(tab) => format!("Preview tab {tab} was not found for {}. Omit tabId to use the current tab, or call preview_open.", operation_name(error.operation)),
                None => format!("No active preview tab was found for {}. Call preview_open first.", operation_name(error.operation)),
            },
            Timeout(error) => format!("Preview automation {} timed out after {}ms.", operation_name(error.operation), error.timeout_ms.0),
            ControlInterrupted(error) => match error.reason.present().copied() {
                Some(PreviewAutomationControlReason::AgentMismatch) => "This browser tab belongs to another agent session or a human. Open your own tab.".into(),
                Some(PreviewAutomationControlReason::HumanControl) => "A human controls this browser tab. Wait until they release control.".into(),
                Some(PreviewAutomationControlReason::TabRequired) => "Multiple browser tabs belong to this session. Pass a tabId from preview_open or from the tabs listed by preview_status.".into(),
                Some(PreviewAutomationControlReason::DialogPending) => "A browser dialog is pending. Read preview_status and resolve it with preview_dialog.".into(),
                Some(PreviewAutomationControlReason::TabLimit) => "Too many server browser tabs are open. Close one with t3_preview_close, or reuse a tabId from preview_status tabs.".into(),
                Some(PreviewAutomationControlReason::Closed) => "This browser tab is closed. Call preview_open.".into(),
                Some(PreviewAutomationControlReason::Interrupted) => "Browser control changed. Take a fresh snapshot before trying again.".into(),
                None => format!("Preview automation {} was interrupted on client {}.", operation_name(error.operation), error.client_id),
            },
            Execution(error) => match error.reason.present() {
                None => format!("Preview automation {} failed on client {}.", operation_name(error.operation), error.client_id),
                Some(reason) => format!("Preview automation {} failed: {reason}", operation_name(error.operation)),
            },
            InvalidSelector(error) => {
                if error.stale_ref.present() == Some(&true) {
                    "This element ref is stale. Navigation, dialogs, and new snapshots replace refs; take a fresh snapshot and use its refs.".into()
                } else if let (Some(kind), Some(length)) = (error.selector_kind.present(), error.selector_length.present()) {
                    format!("Preview automation {} received an invalid {} ({} characters).", operation_name(error.operation), kind.as_str(), length.0)
                } else {
                    format!("Preview automation {} received an invalid selector.", operation_name(error.operation))
                }
            }
            TargetNotEditable(error) => match (error.selector_kind.present().copied(), error.selector_length.present()) {
                (Some(PreviewAutomationEditableTarget::FocusedElement), _) => format!("Preview automation {} requires an editable focused element.", operation_name(error.operation)),
                (Some(kind), Some(length)) => format!("Preview automation {} requires an editable {} ({} characters).", operation_name(error.operation), kind.as_str(), length.0),
                _ => format!("Preview automation {} requires an editable target.", operation_name(error.operation)),
            },
            ResultTooLarge(error) => match error.maximum_bytes.present() {
                None => format!("Preview automation {} produced a result that is too large.", operation_name(error.operation)),
                Some(maximum) => format!("Preview automation {} produced a result larger than {} bytes.", operation_name(error.operation), maximum.0),
            },
            ClientDisconnected(error) => format!("Preview automation client {} disconnected during {}.", error.client_id, operation_name(error.operation)),
            RequestQueueClosed(error) => format!("Preview automation client {} stopped accepting {} requests.", error.client_id, operation_name(error.operation)),
            RemoteUnavailable(error) => format!("Preview automation {} is unavailable on client {}.", operation_name(error.operation), error.client_id),
            MalformedResponse(error) => format!("Preview automation client {} returned a malformed response for {}.", error.client_id, operation_name(error.operation)),
        }
    }
}
