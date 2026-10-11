//! Ed25519 key pairs and the EdDSA JWTs of the relay protocol (`packages/shared/src/relayJwt.ts`,
//! which uses `jose` with PKCS#8 / SPKI PEM keys). Keys are the PEM texts Node's
//! `generateKeyPairSync("ed25519", { pkcs8, spki })` produces, so existing stored keys keep working.

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair as _, UnparsedPublicKey};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

pub const RELAY_LINK_PROOF_TYP: &str = "t3-env-link+jwt";
pub const RELAY_MINT_REQUEST_TYP: &str = "t3-cloud-mint+jwt";
pub const RELAY_HEALTH_REQUEST_TYP: &str = "t3-cloud-health+jwt";
pub const RELAY_MINT_RESPONSE_TYP: &str = "t3-env-mint+jwt";
pub const RELAY_HEALTH_RESPONSE_TYP: &str = "t3-env-health+jwt";
pub const RELAY_ACTIVITY_PUBLISH_TYP: &str = "t3-env-activity+jwt";
pub const RELAY_MANAGED_TUNNEL_RECOVERY_TYP: &str = "t3-env-managed-tunnel-recovery+jwt";
pub const RELAY_HOOK_DELIVERY_TYP: &str = "t3-relay-hook-delivery+jwt";
/// Header carrying the signed proof that a webhook request came from the relay.
pub const RELAY_HOOK_DELIVERY_HEADER: &str = "x-t3-relay-delivery";

/// The environment's signing key pair. The JSON form is what `cloud-link-ed25519-key-pair` holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyPair {
    pub private_key: String,
    pub public_key: String,
}

// DER prefixes of an Ed25519 key: PKCS#8 v1 (`PrivateKeyInfo`) and SubjectPublicKeyInfo.
const PKCS8_PREFIX: [u8; 16] = [
    0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
];
const SPKI_PREFIX: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

/// What went wrong with a key or token, with the `jose` error code the original would report.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("Failed to {operation} relay JWT of type \"{typ}\".")]
pub struct JwtError {
    pub operation: &'static str,
    pub typ: String,
    pub issuer: Option<String>,
    pub audience: Option<String>,
    /// A stable diagnostic code (`RelayJwtError.diagnosticCode`); never copied from message text.
    pub code: &'static str,
}

fn pem_encode(label: &str, der: &[u8]) -> String {
    let body = STANDARD.encode(der);
    let mut out = format!("-----BEGIN {label}-----\n");
    for chunk in body.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(chunk).expect("base64 is ascii"));
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

fn pem_decode(pem: &str, label: &str) -> Option<Vec<u8>> {
    let pem = normalize_pem(pem);
    let body = pem
        .strip_prefix(&format!("-----BEGIN {label}-----"))?
        .strip_suffix(&format!("-----END {label}-----"))?;
    let compact: String = body.chars().filter(|c| !c.is_whitespace()).collect();
    STANDARD.decode(compact).ok()
}

/// Stored PEMs sometimes carry literal `\n` sequences (`normalizePem`).
fn normalize_pem(value: &str) -> String {
    value.replace("\\n", "\n").trim().to_owned()
}

impl KeyPair {
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        let pair = Ed25519KeyPair::from_seed_unchecked(seed).expect("32-byte seed");
        let mut private = PKCS8_PREFIX.to_vec();
        private.extend_from_slice(seed);
        let mut public = SPKI_PREFIX.to_vec();
        public.extend_from_slice(pair.public_key().as_ref());
        Self {
            private_key: pem_encode("PRIVATE KEY", &private),
            public_key: pem_encode("PUBLIC KEY", &public),
        }
    }

    pub fn generate() -> Self {
        let mut seed = [0u8; 32];
        SystemRandom::new()
            .fill(&mut seed)
            .expect("system randomness");
        Self::from_seed(&seed)
    }
}

fn signing_key(private_pem: &str) -> Option<Ed25519KeyPair> {
    let der = pem_decode(private_pem, "PRIVATE KEY")?;
    Ed25519KeyPair::from_pkcs8_maybe_unchecked(&der).ok()
}

/// The 32 raw bytes of an Ed25519 SPKI public key, or `None` for any other key.
fn public_key_bytes(public_pem: &str) -> Option<[u8; 32]> {
    let der = pem_decode(public_pem, "PUBLIC KEY")?;
    let raw = der.strip_prefix(&SPKI_PREFIX)?;
    raw.try_into().ok()
}

/// Whether `public_pem` is a valid Ed25519 public key (`validateCloudMintPublicKey`).
pub fn is_ed25519_public_key(public_pem: &str) -> bool {
    public_key_bytes(public_pem).is_some()
}

fn sign_error(typ: &str, code: &'static str) -> JwtError {
    JwtError {
        operation: "sign",
        typ: typ.to_owned(),
        issuer: None,
        audience: None,
        code,
    }
}

/// `signRelayJwt`: header `{"alg":"EdDSA","typ":typ}`, payload serialised in the order given.
pub fn sign_relay_jwt(private_pem: &str, typ: &str, payload: &Value) -> Result<String, JwtError> {
    let key = signing_key(private_pem).ok_or_else(|| sign_error(typ, "ERR_JOSE_NOT_SUPPORTED"))?;
    if !payload.is_object() {
        return Err(sign_error(typ, "ERR_JWT_INVALID"));
    }
    let header = serde_json::to_string(&json!({ "alg": "EdDSA", "typ": typ })).expect("header");
    let body = serde_json::to_string(payload).expect("payload");
    let signing_input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header),
        URL_SAFE_NO_PAD.encode(body)
    );
    let signature = key.sign(signing_input.as_bytes());
    Ok(format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature.as_ref())
    ))
}

/// What a verification checks (`verifyRelayJwt`). `max_token_age_secs` defaults to five minutes.
pub struct VerifyOptions<'a> {
    pub public_key: &'a str,
    pub token: &'a str,
    pub typ: &'a str,
    pub issuer: &'a str,
    pub audience: &'a str,
    pub now_epoch_seconds: i64,
    pub max_token_age_secs: Option<i64>,
}

const CLOCK_TOLERANCE: i64 = 60;

fn normalize_typ(typ: &str) -> String {
    let lower = typ.to_lowercase();
    lower
        .strip_prefix("application/")
        .map(str::to_owned)
        .unwrap_or(lower)
}

/// Verifies signature, header `typ`, issuer, audience and ages like `jose`'s `jwtVerify` with
/// `algorithms: ["EdDSA"]`, `clockTolerance: 60` and `maxTokenAge`. Returns the claims.
pub fn verify_relay_jwt(options: &VerifyOptions<'_>) -> Result<Map<String, Value>, JwtError> {
    let fail = |code: &'static str| JwtError {
        operation: "verify",
        typ: options.typ.to_owned(),
        issuer: Some(options.issuer.to_owned()),
        audience: Some(options.audience.to_owned()),
        code,
    };
    let public =
        public_key_bytes(options.public_key).ok_or_else(|| fail("ERR_JOSE_NOT_SUPPORTED"))?;
    let parts: Vec<&str> = options.token.split('.').collect();
    let [header_part, payload_part, signature_part] = parts[..] else {
        return Err(fail("ERR_JWS_INVALID"));
    };
    let header: Value = URL_SAFE_NO_PAD
        .decode(header_part)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .filter(Value::is_object)
        .ok_or_else(|| fail("ERR_JWS_INVALID"))?;
    if header.get("alg").and_then(Value::as_str) != Some("EdDSA") {
        return Err(fail(if header.get("alg").is_some() {
            "ERR_JOSE_ALG_NOT_ALLOWED"
        } else {
            "ERR_JWS_INVALID"
        }));
    }
    let signature = URL_SAFE_NO_PAD
        .decode(signature_part)
        .map_err(|_| fail("ERR_JWS_INVALID"))?;
    let signing_input = format!("{header_part}.{payload_part}");
    UnparsedPublicKey::new(&ED25519, public)
        .verify(signing_input.as_bytes(), &signature)
        .map_err(|_| fail("ERR_JWS_SIGNATURE_VERIFICATION_FAILED"))?;
    let claims: Map<String, Value> = URL_SAFE_NO_PAD
        .decode(payload_part)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|value| match value {
            Value::Object(map) => Some(map),
            _ => None,
        })
        .ok_or_else(|| fail("ERR_JWT_INVALID"))?;
    let typ_ok = header
        .get("typ")
        .and_then(Value::as_str)
        .is_some_and(|typ| normalize_typ(typ) == normalize_typ(options.typ));
    if !typ_ok {
        return Err(fail("ERR_JWT_CLAIM_VALIDATION_FAILED"));
    }
    // Required claims: issuer, audience and (because maxTokenAge is always set) iat.
    for claim in ["iat", "aud", "iss"] {
        if !claims.contains_key(claim) {
            return Err(fail("ERR_JWT_CLAIM_VALIDATION_FAILED"));
        }
    }
    if claims.get("iss").and_then(Value::as_str) != Some(options.issuer) {
        return Err(fail("ERR_JWT_CLAIM_VALIDATION_FAILED"));
    }
    let audience_ok = match claims.get("aud") {
        Some(Value::String(aud)) => aud == options.audience,
        Some(Value::Array(auds)) => auds.iter().any(|a| a.as_str() == Some(options.audience)),
        _ => false,
    };
    if !audience_ok {
        return Err(fail("ERR_JWT_CLAIM_VALIDATION_FAILED"));
    }
    let now = options.now_epoch_seconds;
    // jose compares whole seconds from JSON numbers; fractional claims are numbers too.
    let number = |claim: &str| claims.get(claim).map(|value| value.as_f64());
    if let Some(nbf) = number("nbf") {
        let nbf = nbf.ok_or_else(|| fail("ERR_JWT_CLAIM_VALIDATION_FAILED"))?;
        if nbf > (now + CLOCK_TOLERANCE) as f64 {
            return Err(fail("ERR_JWT_CLAIM_VALIDATION_FAILED"));
        }
    }
    let iat = number("iat")
        .flatten()
        .ok_or_else(|| fail("ERR_JWT_CLAIM_VALIDATION_FAILED"))?;
    if let Some(exp) = number("exp") {
        let exp = exp.ok_or_else(|| fail("ERR_JWT_CLAIM_VALIDATION_FAILED"))?;
        if exp <= (now - CLOCK_TOLERANCE) as f64 {
            return Err(fail("ERR_JWT_EXPIRED"));
        }
    }
    let max_age = options.max_token_age_secs.unwrap_or(5 * 60);
    let age = now as f64 - iat;
    if age - CLOCK_TOLERANCE as f64 > max_age as f64 {
        return Err(fail("ERR_JWT_EXPIRED"));
    }
    if age < -(CLOCK_TOLERANCE as f64) {
        return Err(fail("ERR_JWT_CLAIM_VALIDATION_FAILED"));
    }
    Ok(claims)
}

/// `decodeJwt`: the claims of a token without checking anything.
pub fn decode_relay_jwt(token: &str) -> Option<Map<String, Value>> {
    let payload = token.split('.').nth(1)?;
    match serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()? {
        Value::Object(map) => Some(map),
        _ => None,
    }
}

/// The DER of a PEM, for tests that compare with Node's output.
pub fn pem_der(pem: &str, label: &str) -> Option<Vec<u8>> {
    pem_decode(pem, label)
}
