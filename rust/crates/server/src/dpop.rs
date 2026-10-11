//! DPoP proofs (RFC 9449) as the original verifies them: `packages/shared/src/dpop.ts` (the check of one proof),
//! `packages/shared/src/dpopCommon.ts` (the request URL a proof names) and `apps/server/src/auth/dpop.ts` (the
//! failure categories clients see, and the replay marker that makes a proof single-use).
//!
//! A DPoP session is bound to a P-256 key: the token exchange names the key by the thumbprint of the proof it
//! carries, every later request proves possession of that key again, and the proof also hashes the access token
//! (`ath`) so it cannot be replayed with another one. T3 Connect's relay-vouched clients use only this path.
use base64::{
    Engine,
    alphabet::URL_SAFE,
    engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig, general_purpose::URL_SAFE_NO_PAD},
};
use ring::signature::{ECDSA_P256_SHA256_FIXED, UnparsedPublicKey};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::server_secret_store::ServerSecretStore;

const TYP: &str = "dpop+jwt";
const ALG: &str = "ES256";
const DEFAULT_MAX_AGE_SECONDS: i64 = 300;

/// Secret store name prefix of the replay markers; the hourly sweep (`t3_remote::replay`) removes expired ones.
pub const DPOP_REPLAY_MARKER_PREFIX: &str = "dpop-proof-";

/// `DpopVerificationFailureCode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureCode {
    MissingProof,
    MalformedProof,
    KeyMismatch,
    MethodMismatch,
    UrlMismatch,
    AccessTokenHashMismatch,
    TimeWindow,
    InvalidSignature,
    InvalidProof,
}

/// `DpopFailureReason`, the coarser category the client is told (`mapDpopFailureReason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureReason {
    TimeWindow,
    KeyMismatch,
    RequestMismatch,
    TokenMismatch,
    InvalidProof,
    Replay,
}

impl FailureReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TimeWindow => "time_window",
            Self::KeyMismatch => "key_mismatch",
            Self::RequestMismatch => "request_mismatch",
            Self::TokenMismatch => "token_mismatch",
            Self::InvalidProof => "invalid_proof",
            Self::Replay => "replay",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "time_window" => Self::TimeWindow,
            "key_mismatch" => Self::KeyMismatch,
            "request_mismatch" => Self::RequestMismatch,
            "token_mismatch" => Self::TokenMismatch,
            "invalid_proof" => Self::InvalidProof,
            "replay" => Self::Replay,
            _ => return None,
        })
    }
}

impl FailureCode {
    pub fn reason(self) -> FailureReason {
        match self {
            Self::TimeWindow => FailureReason::TimeWindow,
            Self::KeyMismatch => FailureReason::KeyMismatch,
            Self::MethodMismatch | Self::UrlMismatch => FailureReason::RequestMismatch,
            Self::AccessTokenHashMismatch => FailureReason::TokenMismatch,
            Self::MissingProof | Self::MalformedProof | Self::InvalidSignature | Self::InvalidProof => FailureReason::InvalidProof,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verified {
    Ok { thumbprint: String, jti: String, iat: i64 },
    Rejected { code: FailureCode, reason: &'static str },
}

pub struct Proof<'a> {
    pub proof: Option<&'a str>,
    pub method: &'a str,
    pub url: &'a str,
    pub now_epoch_seconds: i64,
    pub expected_thumbprint: Option<&'a str>,
    pub expected_access_token: Option<&'a str>,
}

fn reject(code: FailureCode, reason: &'static str) -> Verified {
    Verified::Rejected { code, reason }
}

/// Effect's base64url decoder reads unpadded and padded text alike.
fn decode(value: &str) -> Option<Vec<u8>> {
    GeneralPurpose::new(&URL_SAFE, GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent))
        .decode(value)
        .ok()
}

fn encode(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// `normalizeDpopHtu`: the URL without its query and fragment, as `URL.toString()` writes it.
pub fn normalize_htu(url: &str) -> Option<String> {
    let mut parsed = url::Url::parse(url).ok()?;
    parsed.set_fragment(None);
    parsed.set_query(None);
    Some(parsed.to_string())
}

/// The base64url SHA-256 of the access token (the proof's `ath`).
pub fn access_token_hash(token: &str) -> String {
    encode(&Sha256::digest(token.as_bytes()))
}

/// `computeDpopJwkThumbprint`: the hash of the four public members in the key order `stableStringify` writes them.
pub fn jwk_thumbprint(crv: &str, kty: &str, x: &str, y: &str) -> String {
    let member = |value: &str| serde_json::to_string(value).expect("a string encodes");
    encode(&Sha256::digest(format!("{{\"crv\":{},\"kty\":{},\"x\":{},\"y\":{}}}", member(crv), member(kty), member(x), member(y)).as_bytes()))
}

fn non_empty<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key)?.as_str().filter(|text| !text.is_empty())
}

/// `verifyDpopProof`. The order of the checks is the original's, so the failure a caller sees is too. The original tells a
/// signature that does not verify from a key or signature that cannot be read; both read as `invalid_proof` to the client.
pub fn verify(input: &Proof<'_>) -> Verified {
    let Some(proof) = input.proof.filter(|proof| !proof.trim().is_empty()) else {
        return reject(FailureCode::MissingProof, "Missing DPoP proof.");
    };
    let parts: Vec<&str> = proof.split('.').collect();
    if parts.len() != 3 || parts.iter().any(|part| part.is_empty()) {
        return reject(FailureCode::MalformedProof, "Invalid DPoP compact JWT.");
    }
    let invalid = || reject(FailureCode::InvalidProof, "Invalid DPoP proof.");
    // A part that is not base64url text, or not JSON, is an unreadable proof; one that reads but does not have the
    // shape is a malformed one.
    let Some(header) = decode(parts[0]).and_then(|bytes| String::from_utf8(bytes).ok()) else { return invalid() };
    let Some(payload) = decode(parts[1]).and_then(|bytes| String::from_utf8(bytes).ok()) else { return invalid() };
    let header: Option<Value> = serde_json::from_str(&header).ok();
    let payload: Option<Value> = serde_json::from_str(&payload).ok();
    // The header: typ, alg and a public P-256 key (never a private one).
    let key = header.as_ref().and_then(|header| {
        let jwk = header.get("jwk")?;
        let shaped = header.get("typ")?.as_str() == Some(TYP)
            && header.get("alg")?.as_str() == Some(ALG)
            && jwk.get("kty")?.as_str() == Some("EC")
            && jwk.get("crv")?.as_str() == Some("P-256")
            && jwk.get("d").is_none();
        shaped.then(|| Some((non_empty(jwk, "x")?, non_empty(jwk, "y")?)))?
    });
    let claims = payload.as_ref().and_then(|payload| {
        let iat = payload.get("iat")?.as_f64().filter(|iat| iat.is_finite() && iat.fract() == 0.0)?;
        let ath = match payload.get("ath") {
            None => None,
            Some(ath) => Some(ath.as_str()?),
        };
        Some((non_empty(payload, "htm")?, non_empty(payload, "htu")?, non_empty(payload, "jti")?, iat as i64, ath))
    });
    let Some((x, y)) = key else { return reject(FailureCode::MalformedProof, "Invalid DPoP JWT header.") };
    let Some((htm, htu, jti, iat, ath)) = claims else { return reject(FailureCode::MalformedProof, "Invalid DPoP JWT payload.") };

    let thumbprint = jwk_thumbprint("P-256", "EC", x, y);
    if input.expected_thumbprint.is_some_and(|expected| !expected.is_empty() && thumbprint != expected) {
        return reject(FailureCode::KeyMismatch, "DPoP key thumbprint mismatch.");
    }
    if htm.to_uppercase() != input.method.to_uppercase() {
        return reject(FailureCode::MethodMismatch, "DPoP method mismatch.");
    }
    if normalize_htu(input.url).is_none_or(|normalized| normalized != htu) {
        return reject(FailureCode::UrlMismatch, "DPoP URL mismatch.");
    }
    if let Some(token) = input.expected_access_token.filter(|token| !token.is_empty()) {
        if ath != Some(access_token_hash(token).as_str()) {
            return reject(FailureCode::AccessTokenHashMismatch, "DPoP access token hash mismatch.");
        }
    }

    // The key is the uncompressed point; the signature is r || s over the SHA-256 of `header.payload`.
    let (Some(x), Some(y), Some(signature)) = (decode(x), decode(y), decode(parts[2])) else { return invalid() };
    if x.len() != 32 || y.len() != 32 || signature.len() != 64 {
        return invalid();
    }
    let mut point = Vec::with_capacity(65);
    point.push(0x04);
    point.extend_from_slice(&x);
    point.extend_from_slice(&y);
    let signed = format!("{}.{}", parts[0], parts[1]);
    if UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, &point).verify(signed.as_bytes(), &signature).is_err() {
        return reject(FailureCode::InvalidSignature, "Invalid DPoP signature.");
    }

    if iat > input.now_epoch_seconds + 5 || input.now_epoch_seconds - iat > DEFAULT_MAX_AGE_SECONDS {
        return reject(FailureCode::TimeWindow, "DPoP proof is outside the allowed time window.");
    }
    Verified::Ok { thumbprint, jti: jti.to_owned(), iat }
}

/// Why a proof was refused, for a handler to map to the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The proof is not acceptable; the client is told the category.
    Invalid { reason: FailureReason, diagnostic: String },
    /// The replay marker could not be recorded (`ServerAuthDpopReplayStateRecordError`).
    Internal(String),
}

/// Where a proof's replay marker is kept: the secret store beside the database, or memory when the store has no
/// directory (an in-memory test store).
pub enum ReplayMarkers<'a> {
    Directory(&'a ServerSecretStore),
    Memory(&'a std::sync::Mutex<std::collections::HashSet<String>>),
}

/// The proof of a request, checked and consumed (`verifyRequestDpopProof`): the thumbprint of its key on success. A proof
/// is single-use, so the second presentation of the same one is a replay.
pub fn verify_and_consume(input: &Proof<'_>, now_iso: &str, markers: ReplayMarkers<'_>) -> Result<String, Refusal> {
    let (thumbprint, jti, iat) = match verify(input) {
        Verified::Ok { thumbprint, jti, iat } => (thumbprint, jti, iat),
        Verified::Rejected { code, reason } => return Err(Refusal::Invalid { reason: code.reason(), diagnostic: reason.to_owned() }),
    };
    let key = format!("{DPOP_REPLAY_MARKER_PREFIX}{}", encode(&Sha256::digest(format!("{thumbprint}:{jti}").as_bytes())));
    let body = ["thumbprint=".to_owned() + &thumbprint, "jti=".to_owned() + &jti, format!("iat={iat}"), format!("consumedAt={now_iso}")].join("\n");
    match markers {
        ReplayMarkers::Directory(secrets) => match secrets.create(&key, body.as_bytes()) {
            Ok(()) => {}
            Err(error) if error.already_exists() => {
                return Err(Refusal::Invalid { reason: FailureReason::Replay, diagnostic: "DPoP proof replayed.".into() });
            }
            Err(_) => return Err(Refusal::Internal("Failed to record DPoP proof replay state.".into())),
        },
        ReplayMarkers::Memory(seen) => {
            if !seen.lock().unwrap().insert(key) {
                return Err(Refusal::Invalid { reason: FailureReason::Replay, diagnostic: "DPoP proof replayed.".into() });
            }
        }
    }
    Ok(thumbprint)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::{
        rand::SystemRandom,
        signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair},
    };

    /// A client key and what it signs.
    pub struct Client {
        pair: EcdsaKeyPair,
        pub x: String,
        pub y: String,
    }

    impl Client {
        pub fn new() -> Self {
            let rng = SystemRandom::new();
            let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
            let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap();
            let point = pair.public_key().as_ref().to_vec();
            Self { x: encode(&point[1..33]), y: encode(&point[33..65]), pair }
        }
        pub fn thumbprint(&self) -> String {
            jwk_thumbprint("P-256", "EC", &self.x, &self.y)
        }
        pub fn proof(&self, header: Value, payload: Value) -> String {
            let signed = format!("{}.{}", encode(header.to_string().as_bytes()), encode(payload.to_string().as_bytes()));
            let signature = self.pair.sign(&SystemRandom::new(), signed.as_bytes()).unwrap();
            format!("{signed}.{}", encode(signature.as_ref()))
        }
        pub fn header(&self) -> Value {
            serde_json::json!({"typ":"dpop+jwt","alg":"ES256","jwk":{"kty":"EC","crv":"P-256","x":self.x,"y":self.y}})
        }
    }

    const NOW: i64 = 1_800_000_000;

    fn claims(url: &str) -> Value {
        serde_json::json!({"htm":"POST","htu":url,"jti":"proof-1","iat":NOW})
    }

    fn check(proof: &str, expected: Option<&str>, token: Option<&str>) -> Verified {
        verify(&Proof { proof: Some(proof), method: "post", url: "http://127.0.0.1:3774/oauth/token?x=1#y", now_epoch_seconds: NOW, expected_thumbprint: expected, expected_access_token: token })
    }

    #[test]
    fn a_proof_for_this_request_verifies_and_names_its_key() {
        let client = Client::new();
        let proof = client.proof(client.header(), claims("http://127.0.0.1:3774/oauth/token"));
        assert_eq!(check(&proof, Some(&client.thumbprint()), None), Verified::Ok { thumbprint: client.thumbprint(), jti: "proof-1".into(), iat: NOW });
    }

    #[test]
    fn each_refusal_carries_the_original_code_and_reason() {
        let client = Client::new();
        let other = Client::new();
        let url = "http://127.0.0.1:3774/oauth/token";
        let code = |proof: &str, expected: Option<&str>, token: Option<&str>| match check(proof, expected, token) {
            Verified::Rejected { code, reason } => (code, reason),
            Verified::Ok { .. } => panic!("accepted"),
        };
        assert_eq!(code("", None, None), (FailureCode::MissingProof, "Missing DPoP proof."));
        assert_eq!(code("a.b", None, None), (FailureCode::MalformedProof, "Invalid DPoP compact JWT."));
        assert_eq!(code("a..c", None, None), (FailureCode::MalformedProof, "Invalid DPoP compact JWT."));
        assert_eq!(code("!.!.!", None, None).0, FailureCode::InvalidProof);
        let proof = client.proof(client.header(), claims(url));
        assert_eq!(code(&proof, Some(&other.thumbprint()), None), (FailureCode::KeyMismatch, "DPoP key thumbprint mismatch."));
        let get = client.proof(client.header(), serde_json::json!({"htm":"GET","htu":url,"jti":"j","iat":NOW}));
        assert_eq!(code(&get, None, None).0, FailureCode::MethodMismatch);
        let elsewhere = client.proof(client.header(), claims("http://127.0.0.1:3774/other"));
        assert_eq!(code(&elsewhere, None, None).0, FailureCode::UrlMismatch);
        assert_eq!(code(&proof, None, Some("token")).0, FailureCode::AccessTokenHashMismatch);
        let mut with_hash = claims(url);
        with_hash["ath"] = access_token_hash("token").into();
        let bound = client.proof(client.header(), with_hash);
        assert!(matches!(check(&bound, None, Some("token")), Verified::Ok { .. }));
        assert_eq!(code(&bound, None, Some("other")).0, FailureCode::AccessTokenHashMismatch);
        let mut old = claims(url);
        old["iat"] = (NOW - 301).into();
        assert_eq!(code(&client.proof(client.header(), old), None, None).0, FailureCode::TimeWindow);
        let mut future = claims(url);
        future["iat"] = (NOW + 6).into();
        assert_eq!(code(&client.proof(client.header(), future), None, None).0, FailureCode::TimeWindow);
        let mut edge = claims(url);
        edge["iat"] = (NOW + 5).into();
        assert!(matches!(check(&client.proof(client.header(), edge), None, None), Verified::Ok { .. }));
        // Signed by a key other than the one the header names.
        let forged = format!("{}.{}", proof.rsplit_once('.').unwrap().0, other.proof(other.header(), claims(url)).rsplit('.').next().unwrap());
        assert_eq!(code(&forged, None, None), (FailureCode::InvalidSignature, "Invalid DPoP signature."));
        // A header that is not a public ES256 key is malformed.
        let mut private = client.header();
        private["jwk"]["d"] = "secret".into();
        assert_eq!(code(&client.proof(private, claims(url)), None, None), (FailureCode::MalformedProof, "Invalid DPoP JWT header."));
        let mut wrong_alg = client.header();
        wrong_alg["alg"] = "RS256".into();
        assert_eq!(code(&client.proof(wrong_alg, claims(url)), None, None).0, FailureCode::MalformedProof);
        assert_eq!(code(&client.proof(client.header(), serde_json::json!({"htm":"POST","htu":url,"jti":"","iat":NOW})), None, None), (FailureCode::MalformedProof, "Invalid DPoP JWT payload."));
        assert_eq!(code(&client.proof(client.header(), serde_json::json!({"htm":"POST","htu":url,"jti":"j","iat":1.5})), None, None).0, FailureCode::MalformedProof);
    }

    #[test]
    fn failure_codes_map_to_the_categories_clients_see() {
        use FailureCode::*;
        let mapped = [
            (TimeWindow, "time_window"),
            (KeyMismatch, "key_mismatch"),
            (MethodMismatch, "request_mismatch"),
            (UrlMismatch, "request_mismatch"),
            (AccessTokenHashMismatch, "token_mismatch"),
            (MissingProof, "invalid_proof"),
            (MalformedProof, "invalid_proof"),
            (InvalidSignature, "invalid_proof"),
            (InvalidProof, "invalid_proof"),
        ];
        for (code, expected) in mapped {
            assert_eq!(code.reason().as_str(), expected);
        }
    }

    #[test]
    fn a_proof_is_single_use() {
        let client = Client::new();
        let proof = client.proof(client.header(), claims("http://127.0.0.1:3774/oauth/token"));
        let input = Proof { proof: Some(&proof), method: "POST", url: "http://127.0.0.1:3774/oauth/token", now_epoch_seconds: NOW, expected_thumbprint: None, expected_access_token: None };
        let seen = std::sync::Mutex::new(Default::default());
        assert_eq!(verify_and_consume(&input, "now", ReplayMarkers::Memory(&seen)), Ok(client.thumbprint()));
        assert_eq!(verify_and_consume(&input, "now", ReplayMarkers::Memory(&seen)), Err(Refusal::Invalid { reason: FailureReason::Replay, diagnostic: "DPoP proof replayed.".into() }));
        // The marker is a file of the secret store, named by the hash of key and proof id.
        let directory = tempfile::Builder::new().prefix("t3code-rust-port-test-dpop-").tempdir().unwrap();
        let secrets = ServerSecretStore::open(directory.path().join("secrets")).unwrap();
        assert_eq!(verify_and_consume(&input, "2026-01-01T00:00:00.000Z", ReplayMarkers::Directory(&secrets)), Ok(client.thumbprint()));
        let names: Vec<_> = std::fs::read_dir(directory.path().join("secrets")).unwrap().map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names.len(), 1);
        assert!(names[0].starts_with("dpop-proof-") && names[0].ends_with(".bin"), "{names:?}");
        let text = String::from_utf8(std::fs::read(directory.path().join("secrets").join(&names[0])).unwrap()).unwrap();
        assert_eq!(text, format!("thumbprint={}\njti=proof-1\niat={NOW}\nconsumedAt=2026-01-01T00:00:00.000Z", client.thumbprint()));
        assert!(matches!(verify_and_consume(&input, "later", ReplayMarkers::Directory(&secrets)), Err(Refusal::Invalid { reason: FailureReason::Replay, .. })));
    }

    #[test]
    fn the_url_a_proof_names_has_no_query_or_fragment() {
        assert_eq!(normalize_htu("https://Example.com:443/a/b?x=1#f").as_deref(), Some("https://example.com/a/b"));
        assert_eq!(normalize_htu("http://127.0.0.1:3774").as_deref(), Some("http://127.0.0.1:3774/"));
        assert_eq!(normalize_htu("not a url"), None);
    }
}
