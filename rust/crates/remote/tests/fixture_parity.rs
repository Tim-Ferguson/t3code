//! Replays the oracle fixture: the unchanged original pure functions
//! (`rust/tools/generate_remote_fixtures.mjs`) against their Rust ports.
mod support;

use std::collections::BTreeMap;

use serde_json::{Value, json};
use support::fixture_rows;
use t3_remote::boot_service::*;
use t3_remote::config::*;
use t3_remote::endpoint_runtime::*;
use t3_remote::jwt::*;
use t3_remote::link_checks::*;
use t3_remote::relay_error::*;
use t3_remote::relay_url::*;
use t3_remote::ssh::*;
use t3_remote::startup::*;
use t3_remote::tailscale::*;
use t3_remote::types::*;

fn s(row: &Value, key: &str) -> String {
    row[key]
        .as_str()
        .unwrap_or_else(|| panic!("{key} in {row}"))
        .to_owned()
}

fn opt(row: &Value, key: &str) -> Option<String> {
    row[key].as_str().map(str::to_owned)
}

#[test]
fn pem_keys_match_nodes_export() {
    let keys = &fixture_rows("keys")[0];
    for name in ["a", "b"] {
        let seed: [u8; 32] = hex(&s(&keys[name], "seedHex")).try_into().unwrap();
        let pair = KeyPair::from_seed(&seed);
        assert_eq!(
            pair.private_key,
            s(&keys[name], "privateKey"),
            "{name} private"
        );
        assert_eq!(
            pair.public_key,
            s(&keys[name], "publicKey"),
            "{name} public"
        );
    }
}

fn hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
        .collect()
}

#[test]
fn signed_tokens_are_byte_identical() {
    let rows = fixture_rows("sign");
    assert!(!rows.is_empty());
    for row in rows {
        let token =
            sign_relay_jwt(&s(&row, "privateKey"), &s(&row, "typ"), &row["payload"]).unwrap();
        assert_eq!(token, s(&row, "token"), "{}", row["payload"]);
    }
}

#[test]
fn unusable_signing_keys_fail() {
    for row in fixture_rows("sign_error") {
        assert_eq!(row["ok"], false);
        assert!(sign_relay_jwt(&s(&row, "privateKey"), "t", &json!({})).is_err());
    }
}

#[test]
fn verification_agrees_with_jose() {
    let rows = fixture_rows("verify");
    assert!(rows.len() > 25);
    for row in rows {
        let name = s(&row, "name");
        let options = VerifyOptions {
            public_key: row["publicKey"].as_str().unwrap(),
            token: row["token"].as_str().unwrap(),
            typ: row["typ"].as_str().unwrap(),
            issuer: row["issuer"].as_str().unwrap(),
            audience: row["audience"].as_str().unwrap(),
            now_epoch_seconds: row["nowEpochSeconds"].as_i64().unwrap(),
            max_token_age_secs: row["maxTokenAge"].as_i64(),
        };
        let result = verify_relay_jwt(&options);
        assert_eq!(result.is_ok(), row["ok"] == true, "{name}: {result:?}");
        match result {
            Ok(claims) => assert_eq!(Value::Object(claims), row["claims"], "{name}"),
            Err(error) => {
                if let Some(code) = row["code"].as_str().filter(|c| c.starts_with("ERR_")) {
                    assert_eq!(error.code, code, "{name}");
                }
            }
        }
    }
}

#[test]
fn relay_urls_normalize_alike() {
    for row in fixture_rows("relay_url") {
        let value = s(&row, "value");
        assert_eq!(
            normalize_secure_relay_url(&value),
            opt(&row, "normalized"),
            "{value:?}"
        );
        assert_eq!(
            is_secure_relay_url(&value),
            row["secure"] == true,
            "{value:?}"
        );
    }
    for row in fixture_rows("issuer") {
        assert_eq!(
            normalize_relay_issuer(&s(&row, "value")),
            s(&row, "normalized")
        );
    }
}

#[test]
fn local_origins_parse_alike() {
    for row in fixture_rows("parse_local_origin") {
        let input = s(&row, "input");
        let parsed = parse_managed_endpoint_local_origin(&input);
        assert_eq!(parsed.is_ok(), row["ok"] == true, "{input:?}: {parsed:?}");
        if let Ok(parsed) = parsed {
            let expected = &row["value"];
            assert_eq!(
                parsed.http_base_url,
                expected["httpBaseUrl"].as_str().unwrap(),
                "{input:?}"
            );
            assert_eq!(
                parsed.ws_base_url,
                expected["wsBaseUrl"].as_str().unwrap(),
                "{input:?}"
            );
            assert_eq!(
                serde_json::to_value(&parsed.origin).unwrap(),
                expected["origin"],
                "{input:?}"
            );
        }
    }
}

#[test]
fn endpoint_origins_and_lifetimes_check_alike() {
    for row in fixture_rows("allowed_origin") {
        let origin = ManagedEndpointOrigin {
            local_http_host: t3_contracts::TrimmedNonEmptyString::new(s(&row, "host")).unwrap(),
            local_http_port: row["port"].as_u64().unwrap() as u16,
        };
        assert_eq!(
            is_allowed_endpoint_origin(&origin, &s(&row, "requestUrl")),
            row["allowed"] == true,
            "{row}"
        );
    }
    for row in fixture_rows("proof_lifetime") {
        let ok = has_bounded_cloud_proof_lifetime(
            row["iat"].as_i64().unwrap(),
            row["exp"].as_i64().unwrap(),
            row["nowSeconds"].as_i64().unwrap(),
        );
        assert_eq!(ok, row["ok"] == true, "{row}");
    }
    for row in fixture_rows("request_url") {
        let request = InboundRequest {
            original_url: s(&row["request"], "originalUrl"),
            headers: row["request"]["headers"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
                .collect::<BTreeMap<_, _>>(),
        };
        assert_eq!(
            request_absolute_url(&request),
            opt(&row, "absolute"),
            "{row}"
        );
        assert_eq!(
            has_forwarded_authority_headers(&request),
            row["forwarded"] == true,
            "{row}"
        );
    }
}

#[test]
fn relay_failures_read_alike() {
    let rows = fixture_rows("relay_response");
    assert!(rows.len() > 30);
    for row in rows {
        let body = s(&row, "body");
        let result = filter_relay_response(
            row["status"].as_u64().unwrap() as u16,
            opt(&row, "cfRay").as_deref(),
            body.as_bytes(),
        );
        assert_eq!(result.is_ok(), row["ok"] == true, "{row}");
        if let Err(error) = result {
            let rejection = match error.rejection {
                Rejection::Unauthorized => "unauthorized",
                Rejection::Forbidden => "forbidden",
                Rejection::Rejected => "rejected",
                Rejection::Unavailable => "unavailable",
            };
            assert_eq!(rejection, s(&row, "rejection"), "{row}");
            assert_eq!(error.description, s(&row, "description"), "{row}");
            assert_eq!(error.should_retry(), row["retry"] == true, "{row}");
        }
    }
    let text = &fixture_rows("relay_error_text")[0];
    assert_eq!(
        RelayRequestError::unexpected_response().description,
        s(text, "unexpected")
    );
}

fn registration(value: &Value) -> StartupRegistration {
    match value["status"].as_str().unwrap() {
        "not_linked" => StartupRegistration::NotLinked,
        "ready" => StartupRegistration::Ready,
        "unavailable" => StartupRegistration::Unavailable,
        "superseded" => StartupRegistration::Superseded,
        "recovery_required" => StartupRegistration::RecoveryRequired {
            config: serde_json::from_value(value["config"].clone()).unwrap(),
        },
        other => panic!("{other}"),
    }
}

#[test]
fn startup_actions_agree() {
    for row in fixture_rows("startup_action") {
        let action = managed_tunnel_startup_action(
            row["wantsCliLink"] == true,
            &registration(&row["registration"]),
        );
        let (name, config) = match &action {
            StartupAction::None => ("none", None),
            StartupAction::ReconcileLink => ("reconcile_link", None),
            StartupAction::RequestRecovery { config } => (
                "request_recovery",
                Some(serde_json::to_value(config).unwrap()),
            ),
        };
        assert_eq!(name, row["action"]["action"].as_str().unwrap(), "{row}");
        if let Some(config) = config {
            assert_eq!(config, row["action"]["config"]);
        }
    }
    let constants = &fixture_rows("startup_constants")[0];
    assert_eq!(
        MANAGED_TUNNEL_RECOVERY_COOLDOWN.as_millis() as u64,
        constants["recoveryCooldownMs"].as_u64().unwrap()
    );
    assert_eq!(
        MANAGED_TUNNEL_FIRST_REGISTRATION_JITTER.as_millis() as u64,
        constants["firstRegistrationJitterMs"].as_u64().unwrap()
    );
}

#[test]
fn connector_output_classifies_alike() {
    for row in fixture_rows("connector_output") {
        let line = s(&row, "line");
        let class = match classify_relay_client_output(&line) {
            OutputClass::Connected => "connected",
            OutputClass::Warning => "warning",
            OutputClass::Debug => "debug",
        };
        assert_eq!(class, s(&row, "class"), "{line:?}");
        assert_eq!(
            is_rejected_relay_client_tunnel_output(&line),
            row["rejected"] == true,
            "{line:?}"
        );
    }
    for row in fixture_rows("retryable_status") {
        assert_eq!(
            is_retryable_status_json(&row["status"]),
            row["retryable"] == true,
            "{row}"
        );
    }
}

#[test]
fn connect_auth_urls_agree() {
    for row in fixture_rows("authorize_url") {
        let url = build_connect_authorize_request_url(
            &s(&row, "hostedAppUrl"),
            &s(&row, "state"),
            &s(&row, "challenge"),
            row["loopbackPort"].as_u64().unwrap() as u16,
        )
        .unwrap();
        assert_eq!(url, s(&row, "url"));
    }
    let redirect = &fixture_rows("redirect_uri")[0];
    assert_eq!(
        connect_loopback_redirect_uri(redirect["port"].as_u64().unwrap() as u16),
        s(redirect, "uri")
    );
    let clerk = &fixture_rows("clerk_authorize_url")[0];
    let scopes: Vec<String> = clerk["scopes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    let scopes: Vec<&str> = scopes.iter().map(String::as_str).collect();
    assert_eq!(
        build_connect_clerk_authorize_url(
            &s(clerk, "authorizationEndpoint"),
            &s(clerk, "clientId"),
            &s(clerk, "redirectUri"),
            &scopes,
            &s(clerk, "state"),
            &s(clerk, "challenge"),
        )
        .unwrap(),
        s(clerk, "url")
    );
    for row in fixture_rows("authorize_request_port") {
        let port = s(&row, "port");
        let expected = row["parsed"]["loopbackPort"].as_u64().map(|p| p as u16);
        assert_eq!(parse_loopback_port(port.trim()), expected, "{port:?}");
    }
    for row in fixture_rows("clerk_key") {
        let key = s(&row, "key");
        let result = clerk_frontend_api_url_from_publishable_key(&key);
        assert_eq!(result.is_ok(), row["ok"] == true, "{key:?}: {result:?}");
        match result {
            Ok(url) => assert_eq!(url, s(&row, "url"), "{key:?}"),
            Err(ClerkKeyError::Decode(prefix)) => {
                assert_eq!(row["tag"], "ClerkPublishableKeyDecodeError", "{key:?}");
                assert_eq!(prefix, s(&row, "keyPrefix"));
            }
            Err(ClerkKeyError::FrontendApi { prefix, reason }) => {
                assert_eq!(row["tag"], "ClerkPublishableKeyFrontendApiError", "{key:?}");
                assert_eq!(
                    (prefix.to_owned(), reason.to_owned()),
                    (s(&row, "keyPrefix"), s(&row, "reason"))
                );
            }
        }
    }
}

fn env_of(row: &Value) -> impl Fn(&str) -> Option<String> + use<> {
    let map: BTreeMap<String, String> = row["env"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
        .collect();
    move |name| map.get(name).cloned()
}

#[test]
fn public_config_reads_alike() {
    for row in fixture_rows("relay_url_config") {
        let result = relay_url_config(&env_of(&row), &s(&row, "fallback"));
        assert_eq!(result.is_ok(), row["ok"] == true, "{row}: {result:?}");
        if let Ok(url) = result {
            assert_eq!(url, s(&row, "value"));
        }
    }
    for row in fixture_rows("hosted_app_url") {
        let result = hosted_app_url_config(&env_of(&row));
        assert_eq!(result.is_ok(), row["ok"] == true, "{row}: {result:?}");
        if let Ok(url) = result {
            assert_eq!(url, s(&row, "value"));
        }
    }
    for row in fixture_rows("oauth_config") {
        let defaults = BuildDefaults {
            relay_url: String::new(),
            clerk_publishable_key: s(&row["defaults"], "clerkPublishableKeyFallback"),
            clerk_cli_oauth_client_id: s(&row["defaults"], "clerkCliOAuthClientIdFallback"),
        };
        let result = cloud_cli_oauth_config(&env_of(&row), &defaults);
        assert_eq!(result.is_ok(), row["ok"] == true, "{row}: {result:?}");
        if let Ok(config) = result {
            let expected = &row["value"];
            assert_eq!(config.token_endpoint, s(expected, "tokenEndpoint"));
            assert_eq!(
                config.device_authorization_endpoint,
                s(expected, "deviceAuthorizationEndpoint")
            );
            assert_eq!(config.client_id, s(expected, "clientId"));
            assert_eq!(
                config.loopback_port as u64,
                expected["loopbackPort"].as_u64().unwrap()
            );
            assert_eq!(config.redirect_uri, s(expected, "redirectUri"));
        }
    }
}

#[test]
fn boot_service_files_render_alike() {
    for row in fixture_rows("boot_service") {
        let plan = BootServicePlan {
            program: row["plan"]["program"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_owned())
                .collect(),
            base_dir: s(&row["plan"], "baseDir"),
            log_path: s(&row["plan"], "logPath"),
            unit_path: s(&row["plan"], "unitPath"),
        };
        assert_eq!(render_boot_service_unit(&plan), s(&row, "unit"));
        assert_eq!(
            render_boot_service_plist(&plan, &s(&row, "homeDir"), &s(&row, "environmentPath")),
            s(&row, "plist")
        );
        assert_eq!(
            boot_service_base_dir_of(&s(&row, "unit")),
            opt(&row, "unitBase")
        );
        assert_eq!(
            boot_service_base_dir_of(&s(&row, "plist")),
            opt(&row, "plistBase")
        );
    }
    for row in fixture_rows("boot_problem") {
        let problem = [
            BootServiceProblem::UserManagerUnavailable,
            BootServiceProblem::LingerUnavailable,
            BootServiceProblem::LingerDisabled,
            BootServiceProblem::ServiceDisabled,
            BootServiceProblem::ServiceStopped,
            BootServiceProblem::RestartPending,
        ]
        .into_iter()
        .find(|p| p.code() == s(&row, "problem"))
        .unwrap();
        assert_eq!(problem.describe(), s(&row, "text"));
    }
}

#[test]
fn tailscale_helpers_agree() {
    for row in fixture_rows("tailscale_ip") {
        assert_eq!(
            is_tailscale_ipv4_address(&s(&row, "address")),
            row["tailnet"] == true,
            "{row}"
        );
    }
    for row in fixture_rows("tailscale_status") {
        let raw = s(&row, "raw");
        let status = parse_tailscale_status(&raw);
        assert_eq!(status.is_ok(), row["ok"] == true, "{raw:?}");
        if let Ok(status) = status {
            assert_eq!(
                status.magic_dns_name,
                opt(&row["status"], "magicDnsName"),
                "{raw:?}"
            );
            let ips: Vec<String> = row["status"]["tailnetIpv4Addresses"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_owned())
                .collect();
            assert_eq!(status.tailnet_ipv4_addresses, ips, "{raw:?}");
        }
        let dns = parse_tailscale_magic_dns_name(&raw);
        assert_eq!(dns.is_ok(), row["dnsOk"] == true, "{raw:?}");
        if let Ok(dns) = dns {
            assert_eq!(dns, opt(&row, "dns"), "{raw:?}");
        }
    }
    for row in fixture_rows("tailscale_url") {
        let port = row["servePort"].as_u64().map(|p| p as u16);
        assert_eq!(
            build_tailscale_https_base_url(&s(&row, "magicDnsName"), port).unwrap(),
            s(&row, "url")
        );
    }
}

fn target_of(value: &Value) -> SshTarget {
    SshTarget {
        alias: s(value, "alias"),
        hostname: s(value, "hostname"),
        username: opt(value, "username"),
        port: value["port"].as_i64(),
    }
}

#[test]
fn ssh_helpers_agree() {
    for row in fixture_rows("ssh_resolve") {
        assert_eq!(
            parse_ssh_resolve_output(&s(&row, "alias"), &s(&row, "stdout")),
            target_of(&row["target"]),
            "{row}"
        );
    }
    for row in fixture_rows("ssh_target") {
        let target = target_of(&row["target"]);
        assert_eq!(target_connection_key(&target), s(&row, "key"));
        let args = |value: &Value| {
            value
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(base_ssh_args(&target, Some("no")), args(&row["args"]));
        assert_eq!(base_ssh_args(&target, None), args(&row["argsDefault"]));
    }
    for row in fixture_rows("ssh_last_line") {
        assert_eq!(
            get_last_non_empty_output_line(&s(&row, "stdout")),
            opt(&row, "line"),
            "{row}"
        );
    }
    for row in fixture_rows("ssh_known_hosts") {
        let expected: Vec<String> = row["hosts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect();
        assert_eq!(parse_known_hosts_hostnames(&s(&row, "raw")), expected);
    }
}
