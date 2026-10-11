//! Whether a webhook request really came through this environment's relay
//! (`scheduledTasks/RelayDeliveryProof.ts`). The relay's delivery id, receive time and trace
//! context arrive as plain headers, and the webhook URL can also be called directly, so they are
//! only trusted alongside a proof the relay signed with its mint key. A request without a valid
//! proof is handled as a direct request.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

use crate::clock::Clock;
use crate::jwt::{
    RELAY_HOOK_DELIVERY_HEADER, RELAY_HOOK_DELIVERY_TYP, VerifyOptions, verify_relay_jwt,
};
use crate::relay_url::normalize_relay_issuer;
use crate::secrets::*;

/// Covers the relay's 24-hour hold plus a margin.
pub const MAX_PROOF_AGE_SECONDS: i64 = 25 * 60 * 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedDelivery {
    pub delivery_id: String,
    pub received_at: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeliveryClaims {
    environment_id: String,
    delivery_id: String,
    received_at: String,
    hook_id: String,
}

/// `headers` use lower-case names. The proof must name exactly this delivery, so it cannot be
/// lifted onto another one.
pub fn verify_relay_delivery(
    secrets: &dyn SecretStore,
    environment_id: &str,
    clock: &dyn Clock,
    headers: &BTreeMap<String, String>,
    hook_id: &str,
) -> Option<VerifiedDelivery> {
    let proof = headers.get(RELAY_HOOK_DELIVERY_HEADER)?;
    let delivery_id = headers.get("x-t3-relay-delivery-id")?;
    let received_at = headers.get("x-t3-relay-received-at")?;
    let public_key = get_string(secrets, CLOUD_MINT_PUBLIC_KEY).ok()??;
    let issuer = match get_string(secrets, RELAY_ISSUER_SECRET).ok()? {
        Some(issuer) => issuer,
        None => get_string(secrets, RELAY_URL_SECRET).ok()??,
    };
    let claims = verify_relay_jwt(&VerifyOptions {
        public_key: &public_key,
        token: proof,
        typ: RELAY_HOOK_DELIVERY_TYP,
        issuer: &normalize_relay_issuer(&issuer),
        audience: &format!("t3-env:{environment_id}"),
        now_epoch_seconds: clock.now_seconds(),
        max_token_age_secs: Some(MAX_PROOF_AGE_SECONDS),
    })
    .ok()?;
    let claims: DeliveryClaims = serde_json::from_value(Value::Object(claims)).ok()?;
    (claims.environment_id == environment_id
        && claims.delivery_id == *delivery_id
        && claims.received_at == *received_at
        && claims.hook_id == hook_id)
        .then(|| VerifiedDelivery {
            delivery_id: delivery_id.clone(),
            received_at: received_at.clone(),
        })
}
