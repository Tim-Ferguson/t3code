//! Public T3 Connect configuration: relay URL, hosted app origin, and the Clerk values the CLI's
//! OAuth flows need (`cloud/publicConfig.ts`, `packages/shared/src/connectAuth.ts`, `relayAuth.ts`).
//! Everything here is a public identifier; none of it is a secret.
//!
//! The original embeds defaults at build time (`__T3CODE_BUILD_*__`) and lets environment
//! variables override them. [`BuildDefaults`] carries the embedded values; an environment lookup
//! is passed in rather than read from the process.

use base64::Engine;
use base64::alphabet;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use url::Url;

use crate::relay_url::normalize_secure_relay_url;

pub const RELAY_URL_ENV: &str = "T3CODE_RELAY_URL";
pub const HOSTED_APP_URL_ENV: &str = "T3CODE_HOSTED_APP_URL";
pub const CLERK_PUBLISHABLE_KEY_ENV: &str = "T3CODE_CLERK_PUBLISHABLE_KEY";
pub const CLERK_CLI_OAUTH_CLIENT_ID_ENV: &str = "T3CODE_CLERK_CLI_OAUTH_CLIENT_ID";
pub const DEFAULT_HOSTED_APP_URL: &str = "https://app.t3.codes";
pub const CLOUD_CLI_OAUTH_LOOPBACK_PORT: u16 = 34338;
/// `offline_access` asks Clerk for the refresh token the CLI relies on.
pub const CONNECT_OAUTH_SCOPES: [&str; 4] = ["openid", "profile", "email", "offline_access"];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ConfigError {
    pub message: String,
}

impl ConfigError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Values the build embeds; empty strings mean "not embedded".
#[derive(Debug, Clone, Default)]
pub struct BuildDefaults {
    pub relay_url: String,
    pub clerk_publishable_key: String,
    pub clerk_cli_oauth_client_id: String,
}

/// A variable counts as set when it is not the empty string; a blank one is used (and then fails
/// validation or trims to nothing), exactly like `Config.NonEmptyString`.
fn env_set(env: &dyn Fn(&str) -> Option<String>, name: &str) -> Option<String> {
    env(name).filter(|value| !value.is_empty())
}

/// The relay origin: `T3CODE_RELAY_URL` over the embedded value, validated as a secure origin.
pub fn relay_url_config(
    env: &dyn Fn(&str) -> Option<String>,
    embedded: &str,
) -> Result<String, ConfigError> {
    let embedded = normalize_secure_relay_url(embedded).unwrap_or_default();
    let value = env_set(env, RELAY_URL_ENV)
        .or_else(|| (!embedded.is_empty()).then_some(embedded))
        .ok_or_else(|| ConfigError::new(format!("Missing {RELAY_URL_ENV}")))?;
    normalize_secure_relay_url(&value)
        .ok_or_else(|| ConfigError::new("Relay URL must be a secure absolute HTTPS origin."))
}

/// The hosted app origin the CLI prints authorization links against.
pub fn hosted_app_url_config(env: &dyn Fn(&str) -> Option<String>) -> Result<String, ConfigError> {
    let value = env_set(env, HOSTED_APP_URL_ENV)
        .map(|value| value.trim().to_owned())
        .unwrap_or_else(|| DEFAULT_HOSTED_APP_URL.to_owned());
    let invalid = || {
        ConfigError::new(
            "Hosted app URL must be an absolute HTTPS origin (or HTTP loopback origin).",
        )
    };
    let url = Url::parse(&value).map_err(|_| invalid())?;
    let loopback_http = url.scheme() == "http"
        && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if (url.scheme() != "https" && !loopback_http)
        || url.path() != "/"
        || url.query().is_some_and(|q| !q.is_empty())
        || url.fragment().is_some_and(|f| !f.is_empty())
    {
        return Err(invalid());
    }
    Ok(url.origin().ascii_serialization())
}

/// `atob` as browsers define it ("forgiving base64"): whitespace ignored, padding optional.
fn atob(value: &str) -> Option<Vec<u8>> {
    let engine = GeneralPurpose::new(
        &alphabet::STANDARD,
        GeneralPurposeConfig::new()
            .with_decode_padding_mode(DecodePaddingMode::Indifferent)
            .with_decode_allow_trailing_bits(true),
    );
    let mut compact: String = value.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    if compact.len() % 4 == 0 {
        compact = compact.trim_end_matches('=').to_owned();
        // At most two padding characters are dropped; a third makes the input invalid.
        if value.chars().filter(|c| !c.is_ascii_whitespace()).count() - compact.len() > 2 {
            return None;
        }
    }
    if compact.len() % 4 == 1 {
        return None;
    }
    engine.decode(compact).ok()
}

/// Why a Clerk publishable key did not yield a frontend API.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClerkKeyError {
    #[error("Failed to decode Clerk publishable key ({0}).")]
    Decode(&'static str),
    #[error("Invalid Clerk frontend API decoded from publishable key ({prefix}; {reason}).")]
    FrontendApi {
        prefix: &'static str,
        reason: &'static str,
    },
}

fn key_prefix(key: &str) -> &'static str {
    if key.starts_with("pk_test_") {
        "pk_test"
    } else if key.starts_with("pk_live_") {
        "pk_live"
    } else {
        "unknown"
    }
}

/// `clerkFrontendApiUrlFromPublishableKey`: the key's third `_`-separated part is the base64 of
/// the frontend API host, with a trailing `$`.
pub fn clerk_frontend_api_url_from_publishable_key(key: &str) -> Result<String, ClerkKeyError> {
    let prefix = key_prefix(key);
    let encoded = key.split('_').skip(2).collect::<Vec<_>>().join("_");
    let bytes = atob(&encoded).ok_or(ClerkKeyError::Decode(prefix))?;
    // `atob` yields a binary string; each byte is one UTF-16 unit.
    let decoded: String = bytes.iter().map(|b| *b as char).collect();
    let frontend_api = decoded.strip_suffix('$').unwrap_or(&decoded);
    let frontend = |reason| ClerkKeyError::FrontendApi { prefix, reason };
    if frontend_api.is_empty() {
        return Err(frontend("empty"));
    }
    if frontend_api.contains('/') {
        return Err(frontend("contains-path"));
    }
    let url = format!("https://{frontend_api}");
    Url::parse(&url).map_err(|_| frontend("invalid-url"))?;
    Ok(url)
}

/// What the CLI's OAuth flows contact: Clerk's token and device-authorization endpoints, and the
/// loopback listener for the authorization-code flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudCliOAuthConfig {
    pub token_endpoint: String,
    pub device_authorization_endpoint: String,
    pub client_id: String,
    pub loopback_port: u16,
    pub redirect_uri: String,
    pub scopes: [&'static str; 4],
}

impl CloudCliOAuthConfig {
    /// The CLI's OAuth application when no Clerk values are configured. Its endpoints are empty, so any use of it fails at once as
    /// a request with no address and nothing is sent. (The original fails such a use with the missing-configuration error.)
    pub fn unconfigured() -> Self {
        Self {
            token_endpoint: String::new(),
            device_authorization_endpoint: String::new(),
            client_id: String::new(),
            loopback_port: CLOUD_CLI_OAUTH_LOOPBACK_PORT,
            redirect_uri: connect_loopback_redirect_uri(CLOUD_CLI_OAUTH_LOOPBACK_PORT),
            scopes: CONNECT_OAUTH_SCOPES,
        }
    }
}

/// The loopback callback URI registered on the Clerk CLI OAuth application.
pub fn connect_loopback_redirect_uri(port: u16) -> String {
    format!("http://127.0.0.1:{port}/callback")
}

pub fn cloud_cli_oauth_config(
    env: &dyn Fn(&str) -> Option<String>,
    defaults: &BuildDefaults,
) -> Result<CloudCliOAuthConfig, ConfigError> {
    // TODO(original quirk, cloud/publicConfig.ts makePublicValueConfig): a value that is only
    // whitespace is accepted and trims to "", so a blank client id yields an empty `clientId`.
    let public_value = |name: &str, fallback: &str| {
        env_set(env, name)
            .or_else(|| (!fallback.is_empty()).then(|| fallback.to_owned()))
            .map(|value| value.trim().to_owned())
            .ok_or_else(|| ConfigError::new(format!("Missing {name}")))
    };
    let publishable_key = public_value(CLERK_PUBLISHABLE_KEY_ENV, &defaults.clerk_publishable_key)?;
    let client_id = public_value(
        CLERK_CLI_OAUTH_CLIENT_ID_ENV,
        &defaults.clerk_cli_oauth_client_id,
    )?;
    let frontend = clerk_frontend_api_url_from_publishable_key(&publishable_key).map_err(|_| {
        ConfigError::new("Failed to derive Clerk Frontend API URL from the publishable key.")
    })?;
    Ok(CloudCliOAuthConfig {
        token_endpoint: format!("{frontend}/oauth/token"),
        device_authorization_endpoint: format!("{frontend}/oauth/device_authorization"),
        client_id,
        loopback_port: CLOUD_CLI_OAUTH_LOOPBACK_PORT,
        redirect_uri: connect_loopback_redirect_uri(CLOUD_CLI_OAUTH_LOOPBACK_PORT),
        scopes: CONNECT_OAUTH_SCOPES,
    })
}

/// Whether T3 Connect is configured at all (`hasCloudPublicConfig`); a fresh clone is not.
pub fn has_cloud_public_config(
    env: &dyn Fn(&str) -> Option<String>,
    defaults: &BuildDefaults,
) -> bool {
    let relay = env(RELAY_URL_ENV)
        .and_then(|value| normalize_secure_relay_url(&value))
        .or_else(|| normalize_secure_relay_url(&defaults.relay_url))
        .is_some();
    let trimmed = |name: &str| env(name).is_some_and(|value| !value.trim().is_empty());
    let key =
        trimmed(CLERK_PUBLISHABLE_KEY_ENV) || !defaults.clerk_publishable_key.trim().is_empty();
    let client = trimmed(CLERK_CLI_OAUTH_CLIENT_ID_ENV)
        || !defaults.clerk_cli_oauth_client_id.trim().is_empty();
    relay && key && client
}

/// The URL the CLI prints for the user to open (`buildConnectAuthorizeRequestUrl`). `state` and
/// `code_challenge` ride the fragment so they never reach the hosted app's server logs.
pub fn build_connect_authorize_request_url(
    hosted_app_url: &str,
    state: &str,
    challenge: &str,
    loopback_port: u16,
) -> Result<String, url::ParseError> {
    let mut url = Url::parse(hosted_app_url)?.join("/connect")?;
    let fragment = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("state", state)
        .append_pair("challenge", challenge)
        .append_pair("port", &loopback_port.to_string())
        .finish();
    url.set_fragment(Some(&fragment));
    Ok(url.to_string())
}

/// `buildConnectClerkAuthorizeUrl`: Clerk's own authorize URL (used by the hosted `/connect` page).
pub fn build_connect_clerk_authorize_url(
    authorization_endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    scopes: &[&str],
    state: &str,
    challenge: &str,
) -> Result<String, url::ParseError> {
    let mut url = Url::parse(authorization_endpoint)?;
    url.query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", &scopes.join(" "))
        .append_pair("state", state)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256");
    Ok(url.to_string())
}

/// `readConnectAuthorizeRequest`'s port rule: one to five digits, 1..=65535.
pub fn parse_loopback_port(value: &str) -> Option<u16> {
    if value.is_empty() || value.len() > 5 || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value
        .parse::<u32>()
        .ok()
        .filter(|p| (1..=65535).contains(p))
        .map(|p| p as u16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn relay_url_prefers_runtime_over_embedded_and_normalizes() {
        assert_eq!(
            relay_url_config(&env(&[]), "https://embedded.example.test///").unwrap(),
            "https://embedded.example.test"
        );
        assert_eq!(
            relay_url_config(
                &env(&[(RELAY_URL_ENV, "https://runtime.example.test///")]),
                "https://embedded.example.test"
            )
            .unwrap(),
            "https://runtime.example.test"
        );
    }

    #[test]
    fn relay_url_is_required_and_must_be_secure() {
        assert!(relay_url_config(&env(&[]), "").is_err());
        assert!(
            relay_url_config(
                &env(&[(RELAY_URL_ENV, "http://runtime.example.test")]),
                "https://embedded.example.test"
            )
            .is_err()
        );
        // An embedded value with a path is dropped, which leaves nothing configured.
        assert!(relay_url_config(&env(&[]), "https://embedded.example.test/path").is_err());
    }

    #[test]
    fn hosted_app_url_is_an_origin() {
        assert_eq!(
            hosted_app_url_config(&env(&[(
                HOSTED_APP_URL_ENV,
                "https://nightly.app.t3.codes"
            )]))
            .unwrap(),
            "https://nightly.app.t3.codes"
        );
        assert_eq!(
            hosted_app_url_config(&env(&[(HOSTED_APP_URL_ENV, "http://localhost:5733")])).unwrap(),
            "http://localhost:5733"
        );
        assert_eq!(
            hosted_app_url_config(&env(&[])).unwrap(),
            DEFAULT_HOSTED_APP_URL
        );
        for value in [
            "app.t3.codes",
            "http://app.t3.codes",
            "https://app.t3.codes/nested",
            "https://app.t3.codes?alias=true",
        ] {
            assert!(
                hosted_app_url_config(&env(&[(HOSTED_APP_URL_ENV, value)])).is_err(),
                "{value}"
            );
        }
    }

    #[test]
    fn derives_clerk_endpoints_from_embedded_values() {
        let defaults = BuildDefaults {
            relay_url: String::new(),
            clerk_publishable_key: "pk_test_Y2xlcmsuZXhhbXBsZS50ZXN0JA==".into(),
            clerk_cli_oauth_client_id: "oauth_client_embedded".into(),
        };
        let config = cloud_cli_oauth_config(&env(&[]), &defaults).unwrap();
        assert_eq!(
            config.token_endpoint,
            "https://clerk.example.test/oauth/token"
        );
        assert_eq!(
            config.device_authorization_endpoint,
            "https://clerk.example.test/oauth/device_authorization"
        );
        assert_eq!(config.client_id, "oauth_client_embedded");
        assert_eq!(config.loopback_port, 34338);
        assert_eq!(config.redirect_uri, "http://127.0.0.1:34338/callback");
        assert_eq!(
            config.scopes,
            ["openid", "profile", "email", "offline_access"]
        );
    }

    #[test]
    fn runtime_clerk_values_win_and_missing_values_fail() {
        let defaults = BuildDefaults {
            clerk_publishable_key: "pk_test_ZW1iZWRkZWQuZXhhbXBsZS50ZXN0JA==".into(),
            clerk_cli_oauth_client_id: "oauth_client_embedded".into(),
            ..Default::default()
        };
        let config = cloud_cli_oauth_config(
            &env(&[
                (
                    CLERK_PUBLISHABLE_KEY_ENV,
                    "pk_test_cnVudGltZS5leGFtcGxlLnRlc3Qk",
                ),
                (CLERK_CLI_OAUTH_CLIENT_ID_ENV, "oauth_client_runtime"),
            ]),
            &defaults,
        )
        .unwrap();
        assert_eq!(
            config.token_endpoint,
            "https://runtime.example.test/oauth/token"
        );
        assert_eq!(config.client_id, "oauth_client_runtime");
        assert!(cloud_cli_oauth_config(&env(&[]), &BuildDefaults::default()).is_err());
    }

    #[test]
    fn malformed_publishable_keys_are_typed_failures() {
        let defaults = BuildDefaults {
            clerk_publishable_key: "pk_test_not-base64!!".into(),
            clerk_cli_oauth_client_id: "oauth_client_embedded".into(),
            ..Default::default()
        };
        let error = cloud_cli_oauth_config(&env(&[]), &defaults).unwrap_err();
        assert_eq!(
            error.message,
            "Failed to derive Clerk Frontend API URL from the publishable key."
        );
        assert_eq!(
            clerk_frontend_api_url_from_publishable_key("pk_live_ab!d"),
            Err(ClerkKeyError::Decode("pk_live"))
        );
    }

    #[test]
    fn loopback_ports_are_one_to_five_digits() {
        assert_eq!(parse_loopback_port("34338"), Some(34338));
        assert_eq!(parse_loopback_port("0"), None);
        assert_eq!(parse_loopback_port("65536"), None);
        assert_eq!(parse_loopback_port("123456"), None);
        assert_eq!(parse_loopback_port("12a"), None);
    }
}
