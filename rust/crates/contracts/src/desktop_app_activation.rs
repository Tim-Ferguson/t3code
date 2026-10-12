//! The line protocol between `t3 app` and the desktop shell's control socket
//! (packages/contracts/src/desktopAppActivation.ts). One request, one response, each a JSON object on one line.
use serde::{Deserialize, Serialize};

pub const DESKTOP_APP_ACTIVATION_PROTOCOL_VERSION: u8 = 1;

/// A string that is not blank once trimmed, kept as it was sent. The original checks the line with `Schema.is`, which tests the
/// decoded side of `TrimmedNonEmptyString` and so neither trims nor rewrites; an id comes back exactly as it went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct NonBlankString(String);

impl NonBlankString {
    pub fn new(value: impl Into<String>) -> Result<Self, crate::ValidationError> {
        let value = value.into();
        if crate::trim_wire_string(&value).is_empty() {
            return Err(crate::ValidationError { expected: "a non-blank string" });
        }
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for NonBlankString {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DesktopAppActivationPlatform {
    Darwin,
    Linux,
    Win32,
}

impl DesktopAppActivationPlatform {
    /// The platform of a Node `process.platform` name; none outside the three desktops.
    pub fn from_node_name(name: &str) -> Option<Self> {
        match name {
            "darwin" => Some(Self::Darwin),
            "linux" => Some(Self::Linux),
            "win32" => Some(Self::Win32),
            _ => None,
        }
    }

    /// The platform this build runs on, by its Node name; none outside the three desktops.
    pub fn current() -> Option<Self> {
        Self::from_node_name(node_platform_name())
    }

    pub fn node_name(self) -> &'static str {
        match self {
            Self::Darwin => "darwin",
            Self::Linux => "linux",
            Self::Win32 => "win32",
        }
    }
}

/// `process.platform` for the platform this build runs on.
pub fn node_platform_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "windows") {
        "win32"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "unknown"
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DesktopAppActivationRequestType {
    #[serde(rename = "open-workspace")]
    OpenWorkspace,
}

/// `{ version: 1, requestId, type: "open-workspace", workspaceRoot, platform }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopAppActivationRequest {
    pub version: u8,
    pub request_id: NonBlankString,
    #[serde(rename = "type")]
    pub request_type: DesktopAppActivationRequestType,
    pub workspace_root: NonBlankString,
    pub platform: DesktopAppActivationPlatform,
}

impl DesktopAppActivationRequest {
    /// Decodes one request line as the original's `Schema.is` does: version 1 only, every field present, other keys ignored.
    pub fn parse(line: &str) -> Result<Self, String> {
        let request: Self = serde_json::from_str(line).map_err(|error| error.to_string())?;
        if request.version != DESKTOP_APP_ACTIVATION_PROTOCOL_VERSION {
            return Err("Expected the activation protocol version 1.".into());
        }
        Ok(request)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DesktopAppActivationErrorCode {
    #[serde(rename = "invalid-request")]
    InvalidRequest,
    #[serde(rename = "renderer-unavailable")]
    RendererUnavailable,
    #[serde(rename = "environment-unavailable")]
    EnvironmentUnavailable,
    #[serde(rename = "platform-mismatch")]
    PlatformMismatch,
    #[serde(rename = "project-create-failed")]
    ProjectCreateFailed,
    #[serde(rename = "thread-open-failed")]
    ThreadOpenFailed,
    #[serde(rename = "request-timeout")]
    RequestTimeout,
    #[serde(rename = "internal-error")]
    InternalError,
}

impl DesktopAppActivationErrorCode {
    /// The code as the wire and `t3 app`'s message spell it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid-request",
            Self::RendererUnavailable => "renderer-unavailable",
            Self::EnvironmentUnavailable => "environment-unavailable",
            Self::PlatformMismatch => "platform-mismatch",
            Self::ProjectCreateFailed => "project-create-failed",
            Self::ThreadOpenFailed => "thread-open-failed",
            Self::RequestTimeout => "request-timeout",
            Self::InternalError => "internal-error",
        }
    }
}

/// `ok: true`, the literal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrueFlag;
impl Serialize for TrueFlag {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bool(true)
    }
}
impl<'de> Deserialize<'de> for TrueFlag {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match bool::deserialize(deserializer)? {
            true => Ok(Self),
            false => Err(serde::de::Error::custom("expected true")),
        }
    }
}

/// `ok: false`, the literal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FalseFlag;
impl Serialize for FalseFlag {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bool(false)
    }
}
impl<'de> Deserialize<'de> for FalseFlag {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match bool::deserialize(deserializer)? {
            false => Ok(Self),
            true => Err(serde::de::Error::custom("expected false")),
        }
    }
}

/// `{ version, requestId, ok: true, projectId, threadId }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopAppActivationSuccess {
    pub version: u8,
    pub request_id: NonBlankString,
    pub ok: TrueFlag,
    pub project_id: NonBlankString,
    pub thread_id: NonBlankString,
}

/// `{ version, requestId, ok: false, code, message }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopAppActivationFailure {
    pub version: u8,
    pub request_id: NonBlankString,
    pub ok: FalseFlag,
    pub code: DesktopAppActivationErrorCode,
    pub message: NonBlankString,
}

/// The two answers: the project and thread the desktop opened, or why it could not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DesktopAppActivationResponse {
    Success(DesktopAppActivationSuccess),
    Failure(DesktopAppActivationFailure),
}

impl DesktopAppActivationResponse {
    pub fn request_id(&self) -> &str {
        match self {
            Self::Success(success) => success.request_id.as_str(),
            Self::Failure(failure) => failure.request_id.as_str(),
        }
    }

    /// The answer of an opened project, as the renderer reports it.
    pub fn success(request_id: &str, project_id: &str, thread_id: &str) -> Result<Self, crate::ValidationError> {
        Ok(Self::Success(DesktopAppActivationSuccess {
            version: DESKTOP_APP_ACTIVATION_PROTOCOL_VERSION,
            request_id: NonBlankString::new(request_id)?,
            ok: TrueFlag,
            project_id: NonBlankString::new(project_id)?,
            thread_id: NonBlankString::new(thread_id)?,
        }))
    }

    /// A failure answer, as the desktop shell builds them. A blank id or message is replaced as the original's literals are
    /// never blank.
    pub fn failure(request_id: &str, code: DesktopAppActivationErrorCode, message: &str) -> Self {
        let non_blank = |value: &str, fallback: &str| NonBlankString::new(value).unwrap_or_else(|_| NonBlankString(fallback.to_owned()));
        Self::Failure(DesktopAppActivationFailure {
            version: DESKTOP_APP_ACTIVATION_PROTOCOL_VERSION,
            request_id: non_blank(request_id, "invalid-request"),
            ok: FalseFlag,
            code,
            message: non_blank(message, "The request failed."),
        })
    }

    /// Decodes one response line as the original's `Schema.is` does.
    pub fn parse(line: &str) -> Result<Self, String> {
        let response: Self = serde_json::from_str(line).map_err(|error| error.to_string())?;
        let version = match &response {
            Self::Success(success) => success.version,
            Self::Failure(failure) => failure.version,
        };
        if version != DESKTOP_APP_ACTIVATION_PROTOCOL_VERSION {
            return Err("Expected the activation protocol version 1.".into());
        }
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_request_decodes_exactly_as_the_original_schema_does() {
        let line = json!({"version":1,"requestId":" abc ","type":"open-workspace","workspaceRoot":"/work","platform":"darwin"}).to_string();
        let request = DesktopAppActivationRequest::parse(&line).unwrap();
        // `Schema.is` neither trims nor rewrites, and ignores keys it does not name.
        assert_eq!((request.request_id.as_str(), request.workspace_root.as_str(), request.platform), (" abc ", "/work", DesktopAppActivationPlatform::Darwin));
        let extra = json!({"version":1,"requestId":"a","type":"open-workspace","workspaceRoot":"/w","platform":"linux","extra":1}).to_string();
        assert!(DesktopAppActivationRequest::parse(&extra).is_ok());
        for bad in [
            json!({"version":2,"requestId":"a","type":"open-workspace","workspaceRoot":"/w","platform":"linux"}),
            json!({"version":1,"requestId":"  ","type":"open-workspace","workspaceRoot":"/w","platform":"linux"}),
            json!({"version":1,"requestId":"a","type":"close-workspace","workspaceRoot":"/w","platform":"linux"}),
            json!({"version":1,"requestId":"a","type":"open-workspace","workspaceRoot":"/w","platform":"freebsd"}),
            json!({"version":1,"requestId":"a","type":"open-workspace","platform":"linux"}),
        ] {
            assert!(DesktopAppActivationRequest::parse(&bad.to_string()).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_response_is_a_success_or_a_failure_and_nothing_between() {
        let success = DesktopAppActivationResponse::parse(&json!({"version":1,"requestId":"a","ok":true,"projectId":"p","threadId":"t"}).to_string()).unwrap();
        assert_eq!(success, DesktopAppActivationResponse::success("a", "p", "t").unwrap());
        let failure = DesktopAppActivationResponse::parse(&json!({"version":1,"requestId":"a","ok":false,"code":"project-create-failed","message":"No."}).to_string()).unwrap();
        assert_eq!(failure, DesktopAppActivationResponse::failure("a", DesktopAppActivationErrorCode::ProjectCreateFailed, "No."));
        assert_eq!(serde_json::to_value(&failure).unwrap(), json!({"version":1,"requestId":"a","ok":false,"code":"project-create-failed","message":"No."}));
        for bad in [
            json!({"invalid":true}),
            json!({"version":1,"requestId":"a","ok":true,"projectId":"p"}),
            json!({"version":1,"requestId":"a","ok":false,"code":"nope","message":"x"}),
            json!({"version":1,"requestId":"a","ok":false,"projectId":"p","threadId":"t"}),
            json!({"version":3,"requestId":"a","ok":true,"projectId":"p","threadId":"t"}),
        ] {
            assert!(DesktopAppActivationResponse::parse(&bad.to_string()).is_err(), "{bad}");
        }
    }
}
