//! The inputs of the operations a browser host runs (packages/contracts/src/previewAutomation.ts).
//!
//! A call reaches a host as the value the tool's own schema decoded it to (the catalog `mcp_tool_schemas.json` holds the checks: exactly one
//! target, at most one selector, a locator that is not blank), so these structs only read the members the server's own browser acts on. They
//! are plain serde structs: an unknown member is ignored, a missing optional member is `None`, and a member of the wrong type fails the decode.
//! `tab_id` is routed by the broker and absent from what a host decodes, but is accepted for a caller that passes the whole call.
//! `locator` and `selector` stand for the same thing: a locator, or a legacy CSS selector, for one element.
use crate::PreviewAutomationColorScheme;
use serde::{Deserialize, Serialize};

/// Where `preview_navigate` goes when it is not given a URL: the page of a dev server in the environment the browser runs in.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "kind")]
pub enum BrowserNavigationTarget {
    #[serde(rename = "url")]
    Url { url: String },
    #[serde(rename = "environment-port")]
    EnvironmentPort {
        port: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        protocol: Option<BrowserNavigationProtocol>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum BrowserNavigationProtocol {
    #[serde(rename = "http")]
    Http,
    #[serde(rename = "https")]
    Https,
}

impl BrowserNavigationProtocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum PreviewNavigationReadiness {
    #[serde(rename = "load")]
    Load,
    #[serde(rename = "domContentLoaded")]
    DomContentLoaded,
    #[serde(rename = "none")]
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PreviewMouseButton {
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PreviewResizeMode {
    Fill,
    Freeform,
    Preset,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PreviewResizeOrientation {
    Portrait,
    Landscape,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum PreviewKeyModifier {
    Alt,
    Control,
    Meta,
    Shift,
}

impl PreviewKeyModifier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Alt => "Alt",
            Self::Control => "Control",
            Self::Meta => "Meta",
            Self::Shift => "Shift",
        }
    }
}

macro_rules! input_struct {
    ($(#[$meta:meta])* pub struct $name:ident { $(pub $field:ident: $type:ty,)* }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
        #[serde(rename_all = "camelCase")]
        pub struct $name {
            $(#[serde(default, skip_serializing_if = "Option::is_none")] pub $field: Option<$type>,)*
        }
    };
}

input_struct! {pub struct PreviewAutomationOpenInput { pub tab_id: String, pub url: String, pub open: bool, pub show: bool, pub reuse_existing_tab: bool, }}
input_struct! {pub struct PreviewAutomationNavigateInput { pub tab_id: String, pub url: String, pub target: BrowserNavigationTarget, pub readiness: PreviewNavigationReadiness, pub timeout_ms: u64, }}
input_struct! {pub struct PreviewAutomationDialogInput { pub tab_id: String, pub accept: bool, pub prompt_text: String, }}
input_struct! {pub struct PreviewAutomationSetColorSchemeInput { pub tab_id: String, pub color_scheme: PreviewAutomationColorScheme, }}
input_struct! {pub struct PreviewAutomationResizeInput {
    pub tab_id: String, pub mode: PreviewResizeMode, pub preset: String, pub width: u32, pub height: u32, pub orientation: PreviewResizeOrientation, pub timeout_ms: u64,
}}
input_struct! {pub struct PreviewAutomationClickInput {
    pub tab_id: String, pub selector: String, pub locator: String, pub x: f64, pub y: f64, pub timeout_ms: u64, pub button: PreviewMouseButton, pub click_count: u8,
}}
input_struct! {pub struct PreviewAutomationHoverInput { pub tab_id: String, pub selector: String, pub locator: String, pub x: f64, pub y: f64, pub timeout_ms: u64, }}
input_struct! {pub struct PreviewAutomationSelectInput { pub tab_id: String, pub selector: String, pub locator: String, pub values: Vec<String>, pub timeout_ms: u64, }}
input_struct! {pub struct PreviewAutomationDragInput { pub tab_id: String, pub source: String, pub target: String, pub timeout_ms: u64, }}
input_struct! {pub struct PreviewAutomationUploadInput { pub tab_id: String, pub paths: Vec<String>, pub selector: String, pub locator: String, pub timeout_ms: u64, }}
input_struct! {pub struct PreviewAutomationTypeInput { pub tab_id: String, pub text: String, pub selector: String, pub locator: String, pub clear: bool, pub timeout_ms: u64, }}
input_struct! {pub struct PreviewAutomationPressInput { pub tab_id: String, pub key: String, pub modifiers: Vec<PreviewKeyModifier>, }}
input_struct! {pub struct PreviewAutomationScrollInput { pub tab_id: String, pub delta_x: f64, pub delta_y: f64, pub selector: String, pub locator: String, }}
input_struct! {pub struct PreviewAutomationEvaluateInput { pub tab_id: String, pub expression: String, pub await_promise: bool, pub return_by_value: bool, }}
input_struct! {pub struct PreviewAutomationWaitForInput { pub tab_id: String, pub selector: String, pub locator: String, pub text: String, pub url_includes: String, pub timeout_ms: u64, }}

impl PreviewAutomationClickInput {
    /// The element the call names, if it names one: its locator, else its selector.
    pub fn target(&self) -> Option<&str> {
        self.locator.as_deref().or(self.selector.as_deref())
    }
}
impl PreviewAutomationHoverInput {
    pub fn target(&self) -> Option<&str> {
        self.locator.as_deref().or(self.selector.as_deref())
    }
}
impl PreviewAutomationSelectInput {
    pub fn target(&self) -> Option<&str> {
        self.locator.as_deref().or(self.selector.as_deref())
    }
}
impl PreviewAutomationUploadInput {
    pub fn target(&self) -> Option<&str> {
        self.locator.as_deref().or(self.selector.as_deref())
    }
}
impl PreviewAutomationTypeInput {
    pub fn target(&self) -> Option<&str> {
        self.locator.as_deref().or(self.selector.as_deref())
    }
}
impl PreviewAutomationScrollInput {
    pub fn target(&self) -> Option<&str> {
        self.locator.as_deref().or(self.selector.as_deref())
    }
}
impl PreviewAutomationWaitForInput {
    pub fn target(&self) -> Option<&str> {
        self.locator.as_deref().or(self.selector.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_call_decodes_to_the_members_a_host_reads() {
        let click: PreviewAutomationClickInput = serde_json::from_value(json!({"locator": "aria-ref=t3-1-e5", "button": "right", "clickCount": 2, "timeoutMs": 500})).unwrap();
        assert_eq!((click.target(), click.button, click.click_count, click.timeout_ms), (Some("aria-ref=t3-1-e5"), Some(PreviewMouseButton::Right), Some(2), Some(500)));
        let legacy: PreviewAutomationHoverInput = serde_json::from_value(json!({"selector": "button.save", "tabId": "tab_1"})).unwrap();
        assert_eq!((legacy.target(), legacy.tab_id.as_deref()), (Some("button.save"), Some("tab_1")));
        let coordinates: PreviewAutomationClickInput = serde_json::from_value(json!({"x": 10.5, "y": 20})).unwrap();
        assert_eq!((coordinates.target(), coordinates.x, coordinates.y), (None, Some(10.5), Some(20.0)));
    }

    #[test]
    fn a_navigation_target_is_a_url_or_an_environment_port() {
        let by_port: PreviewAutomationNavigateInput = serde_json::from_value(json!({"target": {"kind": "environment-port", "port": 5173, "protocol": "https", "path": "/a?b=1"}, "readiness": "domContentLoaded"})).unwrap();
        assert_eq!(by_port.readiness, Some(PreviewNavigationReadiness::DomContentLoaded));
        assert_eq!(by_port.target, Some(BrowserNavigationTarget::EnvironmentPort { port: 5173, protocol: Some(BrowserNavigationProtocol::Https), path: Some("/a?b=1".into()) }));
        let by_url: PreviewAutomationNavigateInput = serde_json::from_value(json!({"target": {"kind": "url", "url": "example.com"}})).unwrap();
        assert_eq!(by_url.target, Some(BrowserNavigationTarget::Url { url: "example.com".into() }));
        assert!(serde_json::from_value::<PreviewAutomationNavigateInput>(json!({"target": {"kind": "environment-port", "port": 70000}})).is_err());
        assert!(serde_json::from_value::<PreviewAutomationPressInput>(json!({"key": "a", "modifiers": ["Hyper"]})).is_err());
    }
}
