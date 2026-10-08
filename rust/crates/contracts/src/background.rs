//! Client activity and background work policy wire contracts.
use crate::{history::object_struct, *};
use serde::{Deserialize, Serialize};
use serde_json::Number;
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum BackgroundScope {
    ServerConfig,
    ProviderStatus {
        #[serde(
            rename = "instanceId",
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_optional"
        )]
        instance_id: Option<ProviderInstanceId>,
    },
    VcsStatus {
        cwd: String,
    },
    GitRefs {
        cwd: String,
    },
    Diagnostics,
    Thread {
        #[serde(rename = "threadId")]
        thread_id: ThreadId,
    },
}
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClientKind {
    Web,
    DesktopRenderer,
    Mobile,
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClientAppState {
    Active,
    Inactive,
    Background,
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClientBatteryState {
    Unknown,
    Unplugged,
    Charging,
    Full,
}
pub type RpcClientId = NonNegativeInt;
pub type ClientActivityClientId = BoundedTrimmedString<128>;
object_struct! {pub struct ClientActivityReportInput {
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub environment_id:Option<EnvironmentId>,
    pub client_id:ClientActivityClientId,pub client_kind:ClientKind,
    pub visible:bool,pub focused:bool,pub recently_interacted:bool,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub app_state:Option<ClientAppState>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub low_power_mode:Option<BackgroundBooleanState>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub battery_state:Option<ClientBatteryState>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub network_type:Option<String>,
    pub scopes:Vec<BackgroundScope>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub ttl_ms:Option<Number>,pub observed_at:UtcDateTime,
}}
object_struct! {pub struct ClientActivityLease {
    pub session_id:AuthSessionId,pub rpc_client_id:RpcClientId,
    pub client_id:ClientActivityClientId,pub client_kind:ClientKind,
    pub visible:bool,pub focused:bool,pub recently_interacted:bool,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub app_state:Option<ClientAppState>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub low_power_mode:Option<BackgroundBooleanState>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub battery_state:Option<ClientBatteryState>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="deserialize_optional")]
    pub network_type:Option<String>,
    pub scopes:Vec<BackgroundScope>,pub updated_at:UtcDateTime,pub expires_at:UtcDateTime,
}}
object_struct! {pub struct BackgroundPolicySnapshot {
    pub host_power:HostPowerSnapshot,pub leases:Vec<ClientActivityLease>,
    pub active_foreground_lease_count:Number,pub active_scope_keys:Vec<String>,
    pub should_run_opportunistic_work:bool,pub updated_at:UtcDateTime,
}}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wire_codecs_match_original_background_contracts() {
        for (index, line) in include_str!("../tests/fixtures/background.jsonl")
            .lines()
            .enumerate()
        {
            let row: serde_json::Value = serde_json::from_str(line).unwrap();
            let input = row["input"].clone();
            macro_rules! decode {
                ($ty:ty) => {
                    serde_json::from_value::<$ty>(input).and_then(serde_json::to_value)
                };
            }
            let output = match row["name"].as_str().unwrap() {
                "BackgroundScope" => decode!(BackgroundScope),
                "ClientKind" => decode!(ClientKind),
                "ClientActivityClientId" => decode!(ClientActivityClientId),
                "ClientActivityReportInput" => decode!(ClientActivityReportInput),
                "ClientActivityLease" => decode!(ClientActivityLease),
                "BackgroundPolicySnapshot" => decode!(BackgroundPolicySnapshot),
                _ => unreachable!(),
            };
            assert_eq!(
                output.is_ok(),
                row["accepted"].as_bool().unwrap(),
                "codec witness {index}: {row}"
            );
            if let Ok(output) = output {
                assert_eq!(output, row["output"], "codec witness {index}: {row}");
            }
        }
    }
}
