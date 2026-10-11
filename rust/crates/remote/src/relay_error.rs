//! How a failed relay request is read and reported (`cloud/relayResponse.ts`). The description is
//! safe to show: it carries the relay's own explanation and trace id, never request details.

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    Unauthorized,
    Forbidden,
    Rejected,
    /// May succeed on retry. Every other kind is a refusal by the relay.
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{description}")]
pub struct RelayRequestError {
    pub rejection: Rejection,
    pub description: String,
}

impl RelayRequestError {
    fn unavailable(detail: &str) -> Self {
        Self {
            rejection: Rejection::Unavailable,
            description: format!(
                "Could not complete the T3 Connect relay request. {detail} Check this machine's network connection and relay availability, then retry."
            ),
        }
    }

    /// A request that failed below HTTP; `reason` is the failure category (`TransportError`).
    pub fn transport(reason: &str) -> Self {
        Self::unavailable(&format!("The relay request failed ({reason})."))
    }

    /// The answer did not decode. A timeout lands here too: the original maps every cause that is
    /// not an HTTP client error to this wording.
    /// TODO(original bug, cloud/relayResponse.ts relayRequestError): a timed-out request reports
    /// "The relay returned an unexpected response." instead of saying it timed out.
    pub fn unexpected_response() -> Self {
        Self::unavailable("The relay returned an unexpected response.")
    }

    /// Whether a failed request may succeed on retry: anything but a relay refusal.
    pub fn should_retry(&self) -> bool {
        self.rejection == Rejection::Unavailable
    }
}

const AUTH_REASONS: [&str; 4] = [
    "missing_bearer",
    "invalid_bearer",
    "invalid_dpop",
    "not_authorized",
];
const DPOP_REASONS: [&str; 6] = [
    "time_window",
    "key_mismatch",
    "request_mismatch",
    "token_mismatch",
    "replay",
    "invalid_proof",
];
const PROOF_INVALID_REASONS: [&str; 6] = [
    "invalid_signature_or_scope",
    "descriptor_mismatch",
    "replayed_nonce",
    "challenge_invalid",
    "origin_not_allowed",
    "endpoint_not_secure",
];
const CONNECT_REASONS: [&str; 8] = [
    "client_proof_key_thumbprint_missing",
    "environment_link_not_found",
    "endpoint_provider_not_managed",
    "managed_endpoint_allocation_not_found",
    "managed_endpoint_base_domain_not_configured",
    "managed_endpoint_allocation_not_ready",
    "managed_endpoint_hostname_invalid",
    "managed_endpoint_mismatch",
];
const ENDPOINT_REASONS: [&str; 2] = ["endpoint_request_failed", "endpoint_response_invalid"];
const LINK_FAILED_REASONS: [&str; 4] = [
    "link_persistence_failed",
    "credential_persistence_failed",
    "replay_persistence_failed",
    "internal_error",
];
const LINK_UNAVAILABLE_REASONS: [&str; 2] = [
    "managed_endpoint_not_configured",
    "managed_endpoint_provisioning_failed",
];
const PUBLISH_INVALID_REASONS: [&str; 2] = ["invalid_signature_or_payload", "replayed_nonce"];
const INTERNAL_REASONS: [&str; 4] = [
    "database_unavailable",
    "persistence_failed",
    "upstream_unavailable",
    "internal_error",
];

/// A decoded `RelayProtectedError`: the tag, the message the original builds, and the trace id.
#[derive(Debug, Clone, PartialEq)]
pub struct ProtectedError {
    pub tag: String,
    pub message: String,
    pub trace_id: String,
}

fn field<'a>(body: &'a Value, name: &str) -> Option<&'a str> {
    body.get(name).and_then(Value::as_str)
}

/// The enum field if present and one of `allowed`; `Err` when present but unknown.
fn reason<'a>(body: &'a Value, allowed: &[&str]) -> Option<&'a str> {
    field(body, "reason").filter(|reason| allowed.contains(reason))
}

/// Decodes the relay's tagged error body; `None` when it is not one of the twelve known shapes.
pub fn decode_protected_error(body: &Value) -> Option<ProtectedError> {
    let tag = field(body, "_tag")?;
    let trace_id = crate::types::trimmed(field(body, "traceId")?)?;
    let code = |expected: &str| (field(body, "code") == Some(expected)).then_some(());
    let message = match tag {
        "RelayAuthInvalidError" => {
            code("auth_invalid")?;
            if let Some(dpop) = body.get("dpopFailureReason") {
                DPOP_REASONS.contains(&dpop.as_str()?).then_some(())?;
            }
            format!(
                "Relay authentication failed: {}",
                reason(body, &AUTH_REASONS)?
            )
        }
        "RelayEnvironmentLinkProofExpiredError" => {
            code("environment_link_proof_expired")?;
            "Relay environment link proof expired".to_owned()
        }
        "RelayEnvironmentLinkProofInvalidError" => {
            code("environment_link_proof_invalid")?;
            format!(
                "Relay environment link proof is invalid: {}",
                reason(body, &PROOF_INVALID_REASONS)?
            )
        }
        "RelayEnvironmentConnectNotAuthorizedError" => {
            code("environment_connect_not_authorized")?;
            match body.get("reason") {
                None => "Relay environment connection is not authorized".to_owned(),
                Some(_) => format!(
                    "Relay environment connection is not authorized: {}",
                    reason(body, &CONNECT_REASONS)?
                ),
            }
        }
        "RelayEnvironmentEndpointUnavailableError" => {
            code("environment_endpoint_unavailable")?;
            format!(
                "Relay environment endpoint is unavailable: {}",
                reason(body, &ENDPOINT_REASONS)?
            )
        }
        "RelayEnvironmentEndpointTimedOutError" => {
            code("environment_endpoint_timed_out")?;
            "Relay environment endpoint request timed out".to_owned()
        }
        "RelayEnvironmentLinkFailedError" => {
            code("environment_link_failed")?;
            format!(
                "Relay environment link failed: {}",
                reason(body, &LINK_FAILED_REASONS)?
            )
        }
        "RelayEnvironmentLinkUnavailableError" => {
            code("environment_link_unavailable")?;
            format!(
                "Relay environment link is unavailable: {}",
                reason(body, &LINK_UNAVAILABLE_REASONS)?
            )
        }
        "RelayEnvironmentLinkLimitExceededError" => {
            code("environment_link_limit_exceeded")?;
            let max = body.get("maxTunnels")?.as_f64()?;
            format!("Relay managed tunnel limit reached: this account allows at most {max} tunnels")
        }
        "RelayAgentActivityPublishProofExpiredError" => {
            code("agent_activity_publish_proof_expired")?;
            "Relay agent activity publish proof expired".to_owned()
        }
        "RelayAgentActivityPublishProofInvalidError" => {
            code("agent_activity_publish_proof_invalid")?;
            format!(
                "Relay agent activity publish proof is invalid: {}",
                reason(body, &PUBLISH_INVALID_REASONS)?
            )
        }
        "RelayInternalError" => {
            code("internal_error")?;
            format!("Relay internal error: {}", reason(body, &INTERNAL_REASONS)?)
        }
        _ => return None,
    };
    Some(ProtectedError {
        tag: tag.to_owned(),
        message,
        trace_id,
    })
}

fn recovery_hint(error: &ProtectedError) -> &'static str {
    match error.tag.as_str() {
        "RelayEnvironmentLinkLimitExceededError" => {
            "Unlink an unused environment in T3 Connect, then restart T3 Code on this machine."
        }
        "RelayAuthInvalidError" => {
            "Run `t3 connect login` to check this machine's authorization. If the stored credential was revoked, sign out with `t3 connect logout`, then run `t3 connect` again. Restart T3 Code after signing in."
        }
        "RelayEnvironmentLinkProofExpiredError" | "RelayEnvironmentLinkProofInvalidError" => {
            "Check this machine's date and time, update T3 Code, then restart it."
        }
        _ => {
            "Retry when the relay is available. If this continues, include the trace ID when reporting it."
        }
    }
}

fn is_ray_id(value: &str) -> bool {
    (1..=128).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// `filterRelayResponse`: `Ok(body)` for a 2xx answer, otherwise the classified failure with the
/// relay's own explanation. `cf_ray` is the `cf-ray` response header, if any.
pub fn filter_relay_response(
    status: u16,
    cf_ray: Option<&str>,
    body: &[u8],
) -> Result<(), RelayRequestError> {
    if (200..300).contains(&status) {
        return Ok(());
    }
    let decoded = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| decode_protected_error(&value));
    let request_id = cf_ray
        .filter(|ray| is_ray_id(ray))
        .map(|ray| format!(" Cloudflare Ray ID: {ray}."))
        .unwrap_or_default();
    let description = match &decoded {
        Some(error) => format!(
            "T3 Connect: {}. {} Trace ID: {}.",
            error.message,
            recovery_hint(error),
            error.trace_id
        ),
        None => format!(
            "T3 Connect relay returned HTTP {status} without a recognized error response. Check relay access and any proxy or firewall restrictions, then restart T3 Code.{request_id}"
        ),
    };
    let rejection = match status {
        401 => Rejection::Unauthorized,
        403 => Rejection::Forbidden,
        400..=499 if status != 408 && status != 429 => Rejection::Rejected,
        _ => Rejection::Unavailable,
    };
    Err(RelayRequestError {
        rejection,
        description,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fail(status: u16, body: Value, ray: Option<&str>) -> RelayRequestError {
        filter_relay_response(status, ray, body.to_string().as_bytes()).unwrap_err()
    }

    #[test]
    fn transport_failure_reports_category_without_details() {
        let error = RelayRequestError::transport("TransportError");
        assert_eq!(error.rejection, Rejection::Unavailable);
        assert!(error.description.contains("TransportError"));
        assert!(error.description.contains("network connection"));
        assert!(!error.description.contains("relay.example.test"));
        assert!(error.should_retry());
    }

    #[test]
    fn reports_the_tunnel_limit_and_relay_trace() {
        let error = fail(
            403,
            json!({"_tag": "RelayEnvironmentLinkLimitExceededError", "code": "environment_link_limit_exceeded", "maxTunnels": 3, "traceId": "trace-limit"}),
            None,
        );
        assert_eq!(error.rejection, Rejection::Forbidden);
        assert!(error.description.contains("at most 3 tunnels"));
        assert!(error.description.contains("Unlink an unused environment"));
        assert!(error.description.contains("Trace ID: trace-limit"));
    }

    #[test]
    fn revoked_authorization_is_actionable_and_not_retryable() {
        let error = fail(
            401,
            json!({"_tag": "RelayAuthInvalidError", "code": "auth_invalid", "reason": "invalid_bearer", "traceId": "trace-auth"}),
            None,
        );
        assert_eq!(error.rejection, Rejection::Unauthorized);
        assert!(error.description.contains("invalid_bearer"));
        assert!(error.description.contains("t3 connect login"));
        assert!(error.description.contains("Trace ID: trace-auth"));
        assert!(!error.should_retry());
    }

    #[test]
    fn unrecognized_denial_does_not_print_the_body() {
        let error = filter_relay_response(
            403,
            Some("abcdef1234-IAD"),
            b"<html>private upstream details</html>",
        )
        .unwrap_err();
        assert_eq!(error.rejection, Rejection::Forbidden);
        assert!(error.description.contains("HTTP 403"));
        assert!(error.description.contains("proxy or firewall"));
        assert!(
            error
                .description
                .contains("Cloudflare Ray ID: abcdef1234-IAD")
        );
        assert!(!error.description.contains("private upstream details"));
        let bad_ray = filter_relay_response(403, Some("bad ray!"), b"").unwrap_err();
        assert!(!bad_ray.description.contains("Ray ID"));
    }

    #[test]
    fn transient_statuses_stay_retryable() {
        for status in [408, 429, 500, 502, 503, 504] {
            let error = filter_relay_response(status, None, b"unavailable").unwrap_err();
            assert_eq!(error.rejection, Rejection::Unavailable, "{status}");
            assert!(error.description.contains(&format!("HTTP {status}")));
        }
        assert_eq!(
            filter_relay_response(404, None, b"").unwrap_err().rejection,
            Rejection::Rejected
        );
    }

    #[test]
    fn keeps_the_failure_reason_and_trace_of_a_cleanup_error() {
        let error = fail(
            500,
            json!({"_tag": "RelayInternalError", "code": "internal_error", "reason": "upstream_unavailable", "traceId": "trace-cleanup"}),
            None,
        );
        assert_eq!(error.rejection, Rejection::Unavailable);
        assert!(error.description.contains("upstream_unavailable"));
        assert!(error.description.contains("Trace ID: trace-cleanup"));
    }

    #[test]
    fn success_leaves_the_body_to_the_caller() {
        assert!(filter_relay_response(200, None, b"{\"ok\":true}").is_ok());
        assert!(filter_relay_response(204, None, b"").is_ok());
    }
}
