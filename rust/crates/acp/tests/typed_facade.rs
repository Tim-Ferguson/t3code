//! The public typed facade must represent already validated source outputs
//! without narrowing legacy/null/future shapes during compatibility conversion.
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use t3_acp::types::*;
fn roundtrip<T: DeserializeOwned + Serialize>(value: &Value, context: &str) {
    let typed: T = serde_json::from_value(value.clone())
        .unwrap_or_else(|error| panic!("{context}: {error}: {value}"));
    assert_eq!(serde_json::to_value(typed).unwrap(), *value, "{context}");
}
#[test]
fn typed_compatibility_records_preserve_actual_source_normalization_outputs() {
    let mut checked = 0;
    for line in include_str!("fixtures/normalization.jsonl").lines() {
        let fixture: Value = serde_json::from_str(line).unwrap();
        let output = &fixture["output"];
        let name = fixture["transform"].as_str().unwrap();
        match name {
            "normalizeInitializeResponse" => roundtrip::<InitializeResponse>(output, name),
            "normalizeV2SessionSetupResponse" => roundtrip::<NewSessionResponse>(output, name),
            "normalizeConfigOption" if !output.is_null() => {
                roundtrip::<SessionConfigOption>(output, name)
            }
            "normalizeSessionUpdate" | "normalizeV1SessionUpdate" => {
                roundtrip::<SessionNotification>(output, name)
            }
            "normalizePermissionRequest" => {
                // Some pure-helper probes deliberately bypass the wire schema.
                // Actual client callbacks are decoded before normalization.
                if t3_acp::schema::decode("v2.RequestPermissionRequest", fixture["input"].clone())
                    .is_err()
                    && t3_acp::schema::decode(
                        "v1.RequestPermissionRequest",
                        fixture["input"].clone(),
                    )
                    .is_err()
                {
                    continue;
                }
                roundtrip::<RequestPermissionRequest>(output, name);
            }
            _ => continue,
        }
        checked += 1;
    }
    assert!(checked >= 63, "Source normalized rows checked: {checked}");
}
#[test]
fn typed_results_accept_all_successful_existing_legacy_source_codec_fixtures() {
    use flate2::read::GzDecoder;
    use std::io::{BufRead, BufReader};
    let reader = BufReader::new(GzDecoder::new(
        &include_bytes!("fixtures/source-codecs.jsonl.gz")[..],
    ));
    let mut checked = 0;
    for line in reader.lines() {
        let fixture: Value = serde_json::from_str(&line.unwrap()).unwrap();
        if fixture["valid"] != true {
            continue;
        }
        let value = &fixture["output"];
        let name = fixture["schema"].as_str().unwrap();
        match name {
            "v1.InitializeResponse" => roundtrip::<InitializeResponse>(value, name),
            "compat.NewSessionResponse" | "compat.ForkSessionResponse" => {
                roundtrip::<NewSessionResponse>(value, name)
            }
            "compat.LoadSessionResponse" | "compat.ResumeSessionResponse" => {
                roundtrip::<LoadSessionResponse>(value, name)
            }
            "v1.SetSessionConfigOptionResponse" => {
                roundtrip::<SetSessionConfigOptionResponse>(value, name)
            }
            "v1.PromptResponse" => roundtrip::<PromptResponse>(value, name),
            "v1.AuthenticateResponse" => roundtrip::<AuthenticateResponse>(value, name),
            "v1.LogoutResponse" => roundtrip::<LogoutResponse>(value, name),
            "v1.ListSessionsResponse" => roundtrip::<t3_acp::v2::ListSessionsResponse>(value, name),
            "v1.CloseSessionResponse" => roundtrip::<t3_acp::v2::CloseSessionResponse>(value, name),
            "v2.DeleteSessionResponse" => {
                roundtrip::<t3_acp::v2::DeleteSessionResponse>(value, name)
            }
            "v2.ListProvidersResponse" => {
                roundtrip::<t3_acp::v2::ListProvidersResponse>(value, name)
            }
            "v2.SetProviderResponse" => roundtrip::<t3_acp::v2::SetProviderResponse>(value, name),
            "v2.DisableProviderResponse" => {
                roundtrip::<t3_acp::v2::DisableProviderResponse>(value, name)
            }
            "v1.SetSessionModeResponse" => {
                roundtrip::<t3_acp::v1::SetSessionModeResponse>(value, name)
            }
            "compat.SetSessionModelResponse" => roundtrip::<SetSessionModelResponse>(value, name),
            _ => continue,
        }
        checked += 1;
    }
    assert!(
        checked > 20,
        "Expected substantive legacy fixture coverage, got {checked}"
    );
}

struct FixturePeer {
    calls: std::sync::Mutex<std::collections::VecDeque<Value>>,
    events: tokio::sync::broadcast::Sender<t3_acp::PeerEvent>,
}
impl t3_acp::Peer for FixturePeer {
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<t3_acp::PeerEvent> {
        self.events.subscribe()
    }
    fn request<'a>(
        &'a self,
        method: &'a str,
        params: Value,
        _: std::time::Duration,
    ) -> futures_util::future::BoxFuture<'a, Result<Value, t3_acp::AcpError>> {
        Box::pin(async move {
            let fixture = self
                .calls
                .lock()
                .unwrap()
                .pop_front()
                .expect("Unexpected typed wire call");
            assert_eq!(fixture["wire"]["method"], method, "{}", fixture["method"]);
            assert_eq!(fixture["wire"]["params"], params, "{}", fixture["method"]);
            if fixture["method"] == "prompt" && fixture["generation"] == 2 {
                self.events.send(t3_acp::PeerEvent::Notification {method:"session/update".into(),params:serde_json::json!({"sessionId":"s","update":{"sessionUpdate":"state_update","state":"idle","stopReason":"end_turn","usage":null,"_meta":null}})}).unwrap();
            }
            Ok(fixture["response"].clone())
        })
    }
    fn notify<'a>(
        &'a self,
        method: &'a str,
        params: Value,
    ) -> futures_util::future::BoxFuture<'a, Result<(), t3_acp::AcpError>> {
        Box::pin(async move {
            let fixture = self
                .calls
                .lock()
                .unwrap()
                .pop_front()
                .expect("Unexpected typed notification");
            assert_eq!(fixture["wire"]["method"], method);
            assert_eq!(fixture["wire"]["params"], params);
            Ok(())
        })
    }
    fn respond<'a>(
        &'a self,
        _: t3_acp::RequestId,
        _: Result<Value, t3_acp::RpcError>,
    ) -> futures_util::future::BoxFuture<'a, Result<(), t3_acp::AcpError>> {
        Box::pin(async { panic!("Unexpected callback") })
    }
}
fn typed<T: DeserializeOwned>(value: &Value) -> T {
    serde_json::from_value(value.clone()).unwrap()
}
async fn invoke_typed(
    client: &t3_acp::Client,
    method: &str,
    input: &Value,
) -> Result<Value, t3_acp::AcpError> {
    macro_rules! invoke {
        ($name:ident) => {
            client
                .$name(typed(input))
                .await
                .map(|result| serde_json::to_value(result).unwrap())
        };
    }
    match method {
        "initialize" => invoke!(initialize_typed),
        "authenticate" => invoke!(authenticate_typed),
        "logout" => invoke!(logout_typed),
        "createSession" => invoke!(create_session_typed),
        "loadSession" => invoke!(load_session_typed),
        "listSessions" => invoke!(list_sessions_typed),
        "forkSession" => invoke!(fork_session_typed),
        "resumeSession" => invoke!(resume_session_typed),
        "closeSession" => invoke!(close_session_typed),
        "deleteSession" => invoke!(delete_session_typed),
        "listProviders" => invoke!(list_providers_typed),
        "setProvider" => invoke!(set_provider_typed),
        "disableProvider" => invoke!(disable_provider_typed),
        "setSessionModel" => invoke!(set_session_model_typed),
        "setSessionMode" => invoke!(set_session_mode_typed),
        "setSessionConfigOption" => invoke!(set_session_config_option_typed),
        "prompt" => invoke!(prompt_typed),
        "cancel" => client
            .cancel_typed(typed(input))
            .await
            .map(|()| Value::Null),
        _ => panic!("Uncovered original method: {method}"),
    }
}
#[tokio::test]
async fn all_eighteen_typed_operations_match_original_negotiated_v1_and_v2_calls() {
    let fixtures = include_str!("fixtures/client-facade.jsonl")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    for generation in [1, 2] {
        let cases = fixtures
            .iter()
            .filter(|fixture| fixture["generation"] == generation)
            .collect::<Vec<_>>();
        let (events, _) = tokio::sync::broadcast::channel(32);
        let peer = std::sync::Arc::new(FixturePeer {
            calls: std::sync::Mutex::new(
                cases
                    .iter()
                    .filter(|fixture| fixture["valid"] == true)
                    .map(|fixture| (*fixture).clone())
                    .collect(),
            ),
            events,
        });
        let client = t3_acp::Client::new(peer.clone(), std::time::Duration::from_secs(3));
        for fixture in cases {
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                invoke_typed(
                    &client,
                    fixture["method"].as_str().unwrap(),
                    &fixture["input"],
                ),
            )
            .await
            .unwrap();
            if fixture["valid"] == true {
                assert_eq!(
                    result.unwrap(),
                    fixture["output"],
                    "gen{generation} {}",
                    fixture["method"]
                );
            } else {
                assert!(
                    matches!(
                        result,
                        Err(t3_acp::AcpError::Request(t3_acp::RpcError {
                            code: -32601,
                            ..
                        }))
                    ),
                    "{result:?}"
                );
            }
        }
        assert!(peer.calls.lock().unwrap().is_empty());
    }
}

#[test]
fn typed_mcp_server_preserves_explicit_future_discriminator_and_opaque_fields() {
    let value = serde_json::json!({"type":"future_server","name":"server","command":"looks-like-stdio","future":{"nested":true},"_meta":null});
    assert!(t3_acp::schema::decode("v2.McpServer", value.clone()).is_ok());
    roundtrip::<McpServer>(&value, "future server");
    assert!(matches!(typed::<McpServer>(&value), McpServer::V2(_)));
}
