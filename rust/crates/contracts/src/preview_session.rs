//! Preview sessions (packages/contracts/src/preview.ts): the per-thread browser tabs the server
//! tracks, the RPCs that change them and the event stream that follows them.
use crate::{
    BoundedString, BoundedTrimmedString, BrowserProfileId, NonNegativeInt, PositiveInt, PreviewAppearancePreference, PreviewViewportSetting, PreviewZoomFactor, RangeInt,
    SafeInt, ThreadId, TrimmedNonEmptyString, ValidationError, deserialize_optional, history::object_struct, string_type,
};
use serde::{Deserialize, Serialize};
use serde_json::{Number, Value};

// The viewport, appearance and zoom types are the ones the browser defaults in client settings use (`client_settings.rs`).

pub const PREVIEW_URL_MAX_LENGTH: usize = 2_048;
/// The built-in browser profile whose storage is thrown away with the browser (packages/contracts/src/browserProfile.ts).
pub const INCOGNITO_BROWSER_PROFILE_ID: &str = "incognito";
pub const PREVIEW_STREAM_HOST_SETUP_CLOSE_CODE: u16 = 4503;
const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

fn tab_id(value: &str) -> Result<(), ValidationError> {
    if !value.is_empty() && value.encode_utf16().count() <= 128 {
        Ok(())
    } else {
        Err(ValidationError { expected: "a nonempty preview tab id of at most 128 UTF-16 units" })
    }
}
string_type!(PreviewTabId, tab_id);

/// A preview URL as the contract bounds it: trimmed, non-empty, at most 2048 UTF-16 units.
pub type PreviewUrl = BoundedTrimmedString<PREVIEW_URL_MAX_LENGTH>;
/// A page title: unlike a URL it is neither trimmed nor required.
pub type PreviewTitle = BoundedString<512>;

/// The value of a `Schema.optional` field. The original's codec keeps a present-but-undefined member apart from an absent one
/// (it writes it back as `null`), so such a field is an `Option<Option<T>>`; both read as no value here.
pub trait Present<T> {
    fn present(&self) -> Option<&T>;
}
impl<T> Present<T> for Option<Option<T>> {
    fn present(&self) -> Option<&T> {
        self.as_ref().and_then(Option::as_ref)
    }
}

/// `Schema.Defect` through the JSON codec: an object with a string `message` becomes an error that is written back as its
/// `name` (`Error` unless it names one), `message` and, if it has one, `cause`; any other value is kept as it is.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(transparent)]
pub struct Defect(pub Value);
impl Defect {
    fn normalize(value: Value) -> Value {
        let Value::Object(mut object) = value else { return value };
        let Some(message @ Value::String(_)) = object.get("message").cloned() else { return Value::Object(object) };
        let name = match object.get("name") {
            Some(name @ Value::String(_)) => name.clone(),
            _ => Value::String("Error".into()),
        };
        let mut error = serde_json::Map::new();
        error.insert("name".into(), name);
        error.insert("message".into(), message);
        if let Some(cause) = object.remove("cause") {
            error.insert("cause".into(), Self::normalize(cause));
        }
        Value::Object(error)
    }
}
impl<'de> Deserialize<'de> for Defect {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Value::deserialize(d).map(|value| Self(Self::normalize(value)))
    }
}

/// A string that is exactly one value on the wire, such as a `_tag` or a `type`.
macro_rules! literal {
    ($(#[$meta:meta])* $name:ident = $value:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
        pub struct $name;
        impl Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str($value)
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                if String::deserialize(d)? == $value {
                    Ok(Self)
                } else {
                    Err(serde::de::Error::custom(concat!("expected ", $value)))
                }
            }
        }
    };
}
pub(crate) use literal;

/// A union of structs told apart by one literal member, decoded as Effect decodes it: by that member, and only from an object.
macro_rules! tagged_union {
    ($(#[$meta:meta])* $name:ident by $key:literal { $($variant:ident($payload:ty) => $tag:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Serialize)]
        #[serde(untagged)]
        pub enum $name { $($variant($payload),)+ }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let value = serde_json::Value::deserialize(d)?;
                match value.get($key).and_then(serde_json::Value::as_str) {
                    $(Some($tag) => serde_json::from_value(value).map(Self::$variant).map_err(serde::de::Error::custom),)+
                    _ => Err(serde::de::Error::custom(concat!("expected one of the ", stringify!($name), " members"))),
                }
            }
        }
    };
}
pub(crate) use tagged_union;

macro_rules! vocabulary {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $value:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
        pub enum $name { $(#[serde(rename = $value)] $variant,)+ }
        impl $name {
            pub fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $value,)+ }
            }
        }
    };
}
pub(crate) use vocabulary;

object_struct! {
/// The measured viewport of a page, which can be smaller than any size a user picks.
pub struct PreviewRenderedViewportSize {
    pub width:RangeInt<1,MAX_SAFE_INTEGER>,
    pub height:RangeInt<1,MAX_SAFE_INTEGER>,
}}

vocabulary!(
    /// Where a tab's page runs: `desktop` in one client's webview, `server` in the environment's headless browser.
    PreviewRuntime { Desktop => "desktop", Server => "server" }
);
vocabulary!(PreviewClearTarget { Cookies => "cookies", Cache => "cache" });
vocabulary!(PreviewStreamHostSetupNeed { Sandbox => "sandbox", Libraries => "libraries" });
object_struct! {
/// Host setup the server's browser is missing, sent as JSON in the reason of a stream closed with code 4503.
pub struct PreviewStreamHostSetup {pub need:PreviewStreamHostSetupNeed,pub command:String,}}

literal!(PreviewIdleTag = "Idle");
literal!(PreviewLoadingTag = "Loading");
literal!(PreviewSuccessTag = "Success");
literal!(PreviewLoadFailedTag = "LoadFailed");
object_struct! {pub struct PreviewNavIdle {#[serde(rename="_tag")]pub tag:PreviewIdleTag,}}
object_struct! {pub struct PreviewNavLoading {#[serde(rename="_tag")]pub tag:PreviewLoadingTag,pub url:PreviewUrl,pub title:PreviewTitle,}}
object_struct! {pub struct PreviewNavSuccess {#[serde(rename="_tag")]pub tag:PreviewSuccessTag,pub url:PreviewUrl,pub title:PreviewTitle,}}
object_struct! {pub struct PreviewNavLoadFailed {
    #[serde(rename="_tag")]pub tag:PreviewLoadFailedTag,pub url:PreviewUrl,pub title:PreviewTitle,pub code:SafeInt,pub description:String,
}}
tagged_union! {PreviewNavStatus by "_tag" {
    Idle(PreviewNavIdle)=>"Idle",Loading(PreviewNavLoading)=>"Loading",Success(PreviewNavSuccess)=>"Success",LoadFailed(PreviewNavLoadFailed)=>"LoadFailed",
}}
impl PreviewNavStatus {
    pub fn idle() -> Self {
        Self::Idle(PreviewNavIdle { tag: Default::default() })
    }
    pub fn loading(url: PreviewUrl, title: PreviewTitle) -> Self {
        Self::Loading(PreviewNavLoading { tag: Default::default(), url, title })
    }
    pub fn success(url: PreviewUrl, title: PreviewTitle) -> Self {
        Self::Success(PreviewNavSuccess { tag: Default::default(), url, title })
    }
    /// The title of the page, empty before there is one.
    pub fn title(&self) -> &str {
        match self {
            Self::Idle(_) => "",
            Self::Loading(status) => &status.title.0,
            Self::Success(status) => &status.title.0,
            Self::LoadFailed(status) => &status.title.0,
        }
    }
}

object_struct! {pub struct PreviewRevealRequest {pub id:String,pub force:bool,}}
object_struct! {pub struct PreviewSessionSnapshot {
    pub thread_id:TrimmedNonEmptyString,
    pub tab_id:PreviewTabId,
    pub nav_status:PreviewNavStatus,
    pub can_go_back:bool,
    pub can_go_forward:bool,
    /// A snapshot from an older server has none, which means fill-panel mode.
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub viewport:Option<Option<PreviewViewportSetting>>,
    /// Server tabs only; absent means `system`.
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub color_scheme:Option<Option<PreviewAppearancePreference>>,
    /// Server tabs only; absent means 1.
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub zoom_factor:Option<Option<PreviewZoomFactor>>,
    /// Fixed at open: the browser profile the tab's storage belongs to.
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub profile_id:Option<Option<BrowserProfileId>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub runtime:Option<Option<PreviewRuntime>>,
    /// The authenticated provider session that owns an isolated server tab.
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub automation_owner:Option<Option<String>>,
    /// An agent opened this tab and asked to show it, so viewers float it.
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub reveal:Option<Option<bool>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub reveal_request:Option<Option<PreviewRevealRequest>>,
    pub updated_at:String,
}}

object_struct! {pub struct PreviewOpenInput {
    pub thread_id:ThreadId,
    /// Omitted: an empty (Idle) tab the user types into.
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub url:Option<Option<PreviewUrl>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub viewport:Option<Option<PreviewViewportSetting>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub profile_id:Option<Option<BrowserProfileId>>,
    /// Omitted: a desktop tab. `server` requires the `serverBrowser` capability.
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub runtime:Option<Option<PreviewRuntime>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub reveal:Option<Option<bool>>,
}}
object_struct! {pub struct PreviewNavigateInput {
    pub thread_id:ThreadId,pub tab_id:PreviewTabId,pub url:PreviewUrl,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub resolved_title:Option<Option<PreviewTitle>>,
}}
object_struct! {pub struct PreviewReportStatusInput {
    pub thread_id:ThreadId,pub tab_id:PreviewTabId,pub nav_status:PreviewNavStatus,pub can_go_back:bool,pub can_go_forward:bool,
}}
object_struct! {pub struct PreviewRefreshInput {pub thread_id:ThreadId,pub tab_id:PreviewTabId,}}
object_struct! {pub struct PreviewResizeInput {pub thread_id:ThreadId,pub tab_id:PreviewTabId,pub viewport:PreviewViewportSetting,}}
object_struct! {pub struct PreviewAdjustInput {
    pub thread_id:ThreadId,pub tab_id:PreviewTabId,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub color_scheme:Option<Option<PreviewAppearancePreference>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub zoom_factor:Option<Option<PreviewZoomFactor>>,
    /// Reloads past the cache, as Shift+Reload does.
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub hard_reload:Option<Option<bool>>,
    /// Clears the tab's profile's cookies or HTTP cache; its other storage stays.
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub clear:Option<Option<PreviewClearTarget>>,
}}
object_struct! {pub struct PreviewCloseInput {
    pub thread_id:ThreadId,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub tab_id:Option<Option<PreviewTabId>>,
}}
object_struct! {pub struct PreviewClearProfileInput {pub profile_id:BrowserProfileId,}}
object_struct! {pub struct PreviewListInput {pub thread_id:ThreadId,}}
object_struct! {pub struct PreviewListResult {
    pub sessions:Vec<PreviewSessionSnapshot>,
    /// Identifies the server process, so a revision reset is safe.
    pub server_epoch:TrimmedNonEmptyString,
    /// Monotonic across the server's state; rejects a stale list.
    pub revision:NonNegativeInt,
}}

literal!(PreviewOpenedType = "opened");
literal!(PreviewNavigatedType = "navigated");
literal!(PreviewResizedType = "resized");
literal!(PreviewFailedType = "failed");
literal!(PreviewClosedType = "closed");
object_struct! {pub struct PreviewOpenedEvent {
    pub thread_id:TrimmedNonEmptyString,pub tab_id:PreviewTabId,pub created_at:String,pub server_epoch:TrimmedNonEmptyString,pub revision:PositiveInt,
    pub r#type:PreviewOpenedType,pub snapshot:PreviewSessionSnapshot,
}}
object_struct! {pub struct PreviewNavigatedEvent {
    pub thread_id:TrimmedNonEmptyString,pub tab_id:PreviewTabId,pub created_at:String,pub server_epoch:TrimmedNonEmptyString,pub revision:PositiveInt,
    pub r#type:PreviewNavigatedType,pub snapshot:PreviewSessionSnapshot,
}}
object_struct! {pub struct PreviewResizeRequest {
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub hard_reload:Option<Option<bool>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub clear:Option<Option<PreviewClearTarget>>,
}}
object_struct! {pub struct PreviewResizedEvent {
    pub thread_id:TrimmedNonEmptyString,pub tab_id:PreviewTabId,pub created_at:String,pub server_epoch:TrimmedNonEmptyString,pub revision:PositiveInt,
    pub r#type:PreviewResizedType,pub snapshot:PreviewSessionSnapshot,
    /// A one-off action for the server's browser, such as a hard reload.
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub request:Option<Option<PreviewResizeRequest>>,
}}
object_struct! {pub struct PreviewFailedEvent {
    pub thread_id:TrimmedNonEmptyString,pub tab_id:PreviewTabId,pub created_at:String,pub server_epoch:TrimmedNonEmptyString,pub revision:PositiveInt,
    pub r#type:PreviewFailedType,pub url:PreviewUrl,pub title:PreviewTitle,pub code:SafeInt,pub description:String,
}}
object_struct! {pub struct PreviewClosedEvent {
    pub thread_id:TrimmedNonEmptyString,pub tab_id:PreviewTabId,pub created_at:String,pub server_epoch:TrimmedNonEmptyString,pub revision:PositiveInt,
    pub r#type:PreviewClosedType,
}}
tagged_union! {PreviewEvent by "type" {
    Opened(PreviewOpenedEvent)=>"opened",Navigated(PreviewNavigatedEvent)=>"navigated",Resized(PreviewResizedEvent)=>"resized",
    Failed(PreviewFailedEvent)=>"failed",Closed(PreviewClosedEvent)=>"closed",
}}

literal!(PreviewSessionLookupErrorTag = "PreviewSessionLookupError");
literal!(PreviewInvalidUrlErrorTag = "PreviewInvalidUrlError");
literal!(PreviewControlRequiredErrorTag = "PreviewControlRequiredError");
literal!(PreviewClearProfileErrorTag = "PreviewClearProfileError");
vocabulary!(PreviewInvalidUrlReason { Empty => "empty", Parse => "parse", UnsupportedProtocol => "unsupported-protocol", Unexpected => "unexpected" });
object_struct! {pub struct PreviewSessionLookupError {#[serde(rename="_tag")]pub tag:PreviewSessionLookupErrorTag,pub thread_id:String,pub tab_id:String,}}
object_struct! {pub struct PreviewInvalidUrlError {
    #[serde(rename="_tag")]pub tag:PreviewInvalidUrlErrorTag,pub input_length:Number,pub reason:PreviewInvalidUrlReason,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub protocol:Option<Option<String>>,
    pub cause:Defect,
}}
object_struct! {pub struct PreviewControlRequiredError {#[serde(rename="_tag")]pub tag:PreviewControlRequiredErrorTag,pub tab_id:String,}}
object_struct! {pub struct PreviewClearProfileError {#[serde(rename="_tag")]pub tag:PreviewClearProfileErrorTag,pub profile_id:String,pub cause:Defect,}}
tagged_union! {PreviewError by "_tag" {
    SessionLookup(PreviewSessionLookupError)=>"PreviewSessionLookupError",InvalidUrl(PreviewInvalidUrlError)=>"PreviewInvalidUrlError",
    ControlRequired(PreviewControlRequiredError)=>"PreviewControlRequiredError",
}}
impl PreviewSessionLookupError {
    pub fn new(thread_id: &str, tab_id: &str) -> Self {
        Self { tag: Default::default(), thread_id: thread_id.into(), tab_id: tab_id.into() }
    }
    pub fn message(&self) -> String {
        format!("Unknown preview session: thread={}, tab={}", self.thread_id, self.tab_id)
    }
}
impl PreviewControlRequiredError {
    pub fn new(tab_id: &str) -> Self {
        Self { tag: Default::default(), tab_id: tab_id.into() }
    }
    pub fn message(&self) -> &'static str {
        "Take control of the server browser and use its viewer controls."
    }
}
impl PreviewInvalidUrlError {
    pub fn message(&self) -> String {
        let protocol = self.protocol.present().map(|protocol| format!(": {protocol}")).unwrap_or_default();
        format!("Invalid preview URL ({}{protocol}; input length {}).", self.reason.as_str(), self.input_length)
    }
}
impl PreviewClearProfileError {
    pub fn message(&self) -> &'static str {
        "The environment could not delete this browser profile's data."
    }
}
impl PreviewError {
    pub fn message(&self) -> String {
        match self {
            Self::SessionLookup(error) => error.message(),
            Self::InvalidUrl(error) => error.message(),
            Self::ControlRequired(error) => error.message().into(),
        }
    }
}
