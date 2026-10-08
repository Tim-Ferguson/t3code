//! Original desktop supervisor envelope. Credentials travel over inherited IPC.
use crate::{base::deserialize_optional, history::object_struct, *};
use serde::{Deserialize, Serialize};
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DesktopBootstrapMode {
    #[serde(rename = "desktop")]
    Desktop,
}
object_struct! {pub struct DesktopBackendBootstrap {
    pub mode:DesktopBootstrapMode,
    pub no_browser:bool,
    pub port:PortSchema,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub t3_home:Option<Option<String>>,
    pub host:String,
    pub desktop_bootstrap_token:String,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub desktop_bootstrap_secret:Option<String>,
    pub tailscale_serve_enabled:bool,
    pub tailscale_serve_port:PortSchema,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub otlp_traces_url:Option<Option<String>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub otlp_metrics_url:Option<Option<String>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub otlp_logs_url:Option<Option<String>>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub desktop_telemetry_fd:Option<PositiveInt>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub desktop_telemetry_control_fd:Option<PositiveInt>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub desktop_browser_fd:Option<PositiveInt>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub desktop_browser_control_fd:Option<PositiveInt>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub resource_monitor_path:Option<TrimmedNonEmptyString>,
}}
pub const DESKTOP_UPDATE_RESTART_MARKER_FILE: &str = "desktop-update-restart";
