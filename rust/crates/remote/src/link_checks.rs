//! Pure checks behind T3 Connect link proofs and relay requests: which local origin a link may
//! point at, and which scopes and lifetimes a proof claims and accepts (`cloud/linkChecks.ts`).

use std::collections::BTreeMap;
use url::Url;

use crate::types::{LinkProofRequest, ManagedEndpointOrigin, ProviderKind};

const CLOUD_PROOF_MAX_LIFETIME_SECONDS: i64 = 5 * 60;
const CLOUD_PROOF_CLOCK_SKEW_SECONDS: i64 = 60;

fn normalize_hostname(hostname: &str) -> String {
    let lowered = hostname.trim().to_lowercase();
    match lowered.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        Some(inner) => inner.to_owned(),
        None => lowered,
    }
}

fn is_loopback_hostname(hostname: &str) -> bool {
    matches!(
        normalize_hostname(hostname).as_str(),
        "127.0.0.1" | "::1" | "localhost"
    )
}

fn first_forwarded_header_value(value: Option<&str>) -> Option<&str> {
    let first = value?.split(',').next()?.trim();
    (!first.is_empty()).then_some(first)
}

/// The facts about an inbound HTTP request the link-proof route needs.
#[derive(Debug, Clone, Default)]
pub struct InboundRequest {
    /// The request target as the server saw it (`originalUrl`): an absolute URL or a path.
    pub original_url: String,
    /// Lower-cased header names.
    pub headers: BTreeMap<String, String>,
}

impl InboundRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
}

/// `requestAbsoluteUrl`: the request URL, with a relative target resolved against the `Host` header
/// (first value) or `127.0.0.1`.
pub fn request_absolute_url(request: &InboundRequest) -> Option<String> {
    if let Ok(url) = Url::parse(&request.original_url) {
        return Some(url.to_string());
    }
    let host = first_forwarded_header_value(request.header("host")).unwrap_or("127.0.0.1");
    let base = Url::parse(&format!("http://{host}")).ok()?;
    base.join(&request.original_url)
        .ok()
        .map(|url| url.to_string())
}

/// A request that came through a proxy, which a link proof must not be signed for.
pub fn has_forwarded_authority_headers(request: &InboundRequest) -> bool {
    first_forwarded_header_value(request.header("x-forwarded-host")).is_some()
        || first_forwarded_header_value(request.header("x-forwarded-proto")).is_some()
}

fn endpoint_request_port(url: &Url) -> u16 {
    url.port_or_known_default()
        .unwrap_or(if url.scheme() == "https" { 443 } else { 80 })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedLocalOrigin {
    pub http_base_url: String,
    pub ws_base_url: String,
    pub origin: ManagedEndpointOrigin,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("Invalid local origin")]
pub struct InvalidLocalOrigin;

/// `parseManagedEndpointLocalOrigin`: a bare http(s) origin, nothing else.
pub fn parse_managed_endpoint_local_origin(
    local_origin: &str,
) -> Result<ParsedLocalOrigin, InvalidLocalOrigin> {
    let url = Url::parse(local_origin).map_err(|_| InvalidLocalOrigin)?;
    if local_origin != local_origin.trim()
        || (url.scheme() != "http" && url.scheme() != "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some_and(|q| !q.is_empty())
        || url.fragment().is_some_and(|f| !f.is_empty())
        || local_origin.contains('?')
        || local_origin.contains('#')
    {
        return Err(InvalidLocalOrigin);
    }
    let host = url.host_str().ok_or(InvalidLocalOrigin)?;
    let port_suffix = url.port().map(|p| format!(":{p}")).unwrap_or_default();
    let http_base_url = format!("{}://{host}{port_suffix}", url.scheme());
    let ws_scheme = if url.scheme() == "https" { "wss" } else { "ws" };
    let ws_base_url = format!("{ws_scheme}://{host}{port_suffix}");
    Ok(ParsedLocalOrigin {
        http_base_url,
        ws_base_url,
        origin: ManagedEndpointOrigin::new(host, endpoint_request_port(&url)),
    })
}

/// A link proof may only name this server's own loopback origin, and only when the request that
/// asked for it arrived on that same loopback port.
pub fn is_allowed_endpoint_origin(origin: &ManagedEndpointOrigin, request_url: &str) -> bool {
    if !is_loopback_hostname(origin.local_http_host.as_str()) {
        return false;
    }
    let Ok(url) = Url::parse(request_url) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    is_loopback_hostname(host) && origin.local_http_port == endpoint_request_port(&url)
}

/// A managed (Cloudflare tunnel) endpoint is provisioned by the relay and must point at a loopback
/// origin. A manual endpoint is reached out of band (Tailscale) or not advertised at all.
pub fn is_supported_link_provider_kind(request: &LinkProofRequest) -> bool {
    matches!(
        request.endpoint.provider_kind,
        ProviderKind::CloudflareTunnel | ProviderKind::Manual
    )
}

pub fn link_proof_scopes(request: &LinkProofRequest) -> Vec<&'static str> {
    if request.endpoint.provider_kind == ProviderKind::CloudflareTunnel {
        vec!["agent_activity_notifications", "managed_tunnels"]
    } else {
        vec!["agent_activity_notifications"]
    }
}

pub fn has_exact_scope(scopes: &[String], expected: &str) -> bool {
    scopes.len() == 1 && scopes[0] == expected
}

pub fn has_bounded_cloud_proof_lifetime(iat: i64, exp: i64, now_seconds: i64) -> bool {
    exp > iat
        && exp - iat <= CLOUD_PROOF_MAX_LIFETIME_SECONDS
        && iat <= now_seconds + CLOUD_PROOF_CLOCK_SKEW_SECONDS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ManagedEndpoint;
    use t3_contracts::TrimmedNonEmptyString;

    #[test]
    fn accepts_explicit_default_ports() {
        let parsed = parse_managed_endpoint_local_origin("http://127.0.0.1:80").unwrap();
        assert_eq!(parsed.http_base_url, "http://127.0.0.1");
        assert_eq!(parsed.ws_base_url, "ws://127.0.0.1");
        assert_eq!(parsed.origin, ManagedEndpointOrigin::new("127.0.0.1", 80));
        let parsed = parse_managed_endpoint_local_origin("https://127.0.0.1:443").unwrap();
        assert_eq!(parsed.http_base_url, "https://127.0.0.1");
        assert_eq!(parsed.ws_base_url, "wss://127.0.0.1");
        assert_eq!(parsed.origin, ManagedEndpointOrigin::new("127.0.0.1", 443));
    }

    #[test]
    fn rejects_non_origin_urls() {
        for input in [
            "ftp://127.0.0.1:3773",
            "http://user:password@127.0.0.1:3773",
            "http://127.0.0.1:3773/api",
            "http://127.0.0.1:3773?mode=test",
            "http://127.0.0.1:3773#fragment",
        ] {
            assert_eq!(
                parse_managed_endpoint_local_origin(input),
                Err(InvalidLocalOrigin),
                "{input}"
            );
        }
    }

    fn proof_request(kind: ProviderKind) -> LinkProofRequest {
        LinkProofRequest {
            challenge: "challenge".into(),
            relay_issuer: "https://relay.example.test".into(),
            endpoint: ManagedEndpoint {
                http_base_url: TrimmedNonEmptyString::new("http://127.0.0.1:7331").unwrap(),
                ws_base_url: TrimmedNonEmptyString::new("ws://127.0.0.1:7331").unwrap(),
                provider_kind: kind,
            },
            origin: ManagedEndpointOrigin::new("127.0.0.1", 7331),
        }
    }

    #[test]
    fn accepts_managed_and_manual_endpoints_but_not_t3_relay() {
        assert!(is_supported_link_provider_kind(&proof_request(
            ProviderKind::CloudflareTunnel
        )));
        assert!(is_supported_link_provider_kind(&proof_request(
            ProviderKind::Manual
        )));
        assert!(!is_supported_link_provider_kind(&proof_request(
            ProviderKind::T3Relay
        )));
    }

    #[test]
    fn only_tunnel_links_claim_the_managed_tunnel_scope() {
        assert_eq!(
            link_proof_scopes(&proof_request(ProviderKind::CloudflareTunnel)),
            ["agent_activity_notifications", "managed_tunnels"]
        );
        assert_eq!(
            link_proof_scopes(&proof_request(ProviderKind::Manual)),
            ["agent_activity_notifications"]
        );
    }

    #[test]
    fn endpoint_origin_must_be_loopback_on_the_request_port() {
        let origin = ManagedEndpointOrigin::new("127.0.0.1", 3773);
        assert!(is_allowed_endpoint_origin(
            &origin,
            "http://127.0.0.1:3773/api/connect/link-proof"
        ));
        assert!(is_allowed_endpoint_origin(
            &origin,
            "http://localhost:3773/x"
        ));
        assert!(!is_allowed_endpoint_origin(
            &origin,
            "http://127.0.0.1:3774/x"
        ));
        assert!(!is_allowed_endpoint_origin(
            &origin,
            "http://192.168.1.4:3773/x"
        ));
        let remote = ManagedEndpointOrigin::new("example.com", 3773);
        assert!(!is_allowed_endpoint_origin(
            &remote,
            "http://127.0.0.1:3773/x"
        ));
        let v6 = ManagedEndpointOrigin::new("[::1]", 3773);
        assert!(is_allowed_endpoint_origin(&v6, "http://[::1]:3773/x"));
    }

    #[test]
    fn proof_lifetime_is_bounded() {
        let now = 1_000;
        assert!(has_bounded_cloud_proof_lifetime(now, now + 300, now));
        assert!(!has_bounded_cloud_proof_lifetime(now, now + 301, now));
        assert!(!has_bounded_cloud_proof_lifetime(now, now, now));
        assert!(has_bounded_cloud_proof_lifetime(now + 60, now + 90, now));
        assert!(!has_bounded_cloud_proof_lifetime(now + 61, now + 90, now));
    }

    #[test]
    fn exact_scope_needs_exactly_one_matching_scope() {
        assert!(has_exact_scope(&["a".into()], "a"));
        assert!(!has_exact_scope(&["a".into(), "a".into()], "a"));
        assert!(!has_exact_scope(&[], "a"));
    }

    #[test]
    fn forwarded_headers_and_relative_targets() {
        let mut request = InboundRequest {
            original_url: "/api/connect/link-proof".into(),
            ..Default::default()
        };
        request
            .headers
            .insert("host".into(), "127.0.0.1:3773, x".into());
        assert_eq!(
            request_absolute_url(&request).as_deref(),
            Some("http://127.0.0.1:3773/api/connect/link-proof")
        );
        assert!(!has_forwarded_authority_headers(&request));
        request
            .headers
            .insert("x-forwarded-host".into(), " , ".into());
        assert!(!has_forwarded_authority_headers(&request));
        request
            .headers
            .insert("x-forwarded-proto".into(), "https".into());
        assert!(has_forwarded_authority_headers(&request));
    }
}
