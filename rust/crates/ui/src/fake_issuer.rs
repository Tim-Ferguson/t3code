//! A fake OpenAI issuer on a loopback socket, for the tests of the ChatGPT sign-in a managed Codex offers. OpenAI is never
//! contacted: the server under test is pointed at this one (`Environment::discovery_url`). It serves discovery, a signing key,
//! the authorization endpoint (which records the request and redirects the "browser" back with a code), the token endpoint
//! (which checks the PKCE verifier) and the account's model list. A cut-down copy of the issuer of
//! `crates/server/tests/codex_chatgpt_auth_flow.rs`.
use axum::{
    Form, Router,
    extract::{Query, State},
    response::{IntoResponse, Redirect},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

pub(crate) struct Issuer {
    pub origin: String,
    key: EcdsaKeyPair,
    /// The authorization requests it saw, in order.
    pub authorized: Mutex<Vec<HashMap<String, String>>>,
}
type Shared = Arc<Issuer>;

/// A running issuer; dropping it stops the listener.
pub(crate) struct Running {
    pub issuer: Shared,
    task: tokio::task::JoinHandle<()>,
}
impl std::ops::Deref for Running {
    type Target = Issuer;
    fn deref(&self) -> &Issuer {
        &self.issuer
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn b64(bytes: impl AsRef<[u8]>) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

impl Issuer {
    pub(crate) async fn start() -> Running {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let issuer = Arc::new(Issuer { origin, key, authorized: Default::default() });
        let served = issuer.clone();
        let task = tokio::spawn(async move {
            let issuer = served;
            let router = Router::new()
                .route("/.well-known/openid-configuration", get(|State(issuer): State<Shared>| async move { axum::Json(json!({"issuer":issuer.origin,"authorization_endpoint":format!("{}/authorize", issuer.origin),"token_endpoint":format!("{}/token", issuer.origin),"jwks_uri":format!("{}/jwks", issuer.origin),"revocation_endpoint":format!("{}/revoke", issuer.origin)})) }))
                .route("/jwks", get(|State(issuer): State<Shared>| async move {
                    let public = issuer.key.public_key().as_ref();
                    axum::Json(json!({"keys":[{"kty":"EC","crv":"P-256","kid":"k1","use":"sig","alg":"ES256","x":b64(&public[1..33]),"y":b64(&public[33..65])}]}))
                }))
                // The "browser" arrives here with the authorization request and is sent back to the redirect with a code.
                .route("/authorize", get(|State(issuer): State<Shared>, Query(query): Query<HashMap<String, String>>| async move {
                    let redirect = query.get("redirect_uri").map(|redirect| format!("{redirect}?code=good-code&state={}&client_id=oaiapp_test", query["state"]));
                    issuer.authorized.lock().unwrap().push(query);
                    match redirect {
                        Some(redirect) => Redirect::to(&redirect).into_response(),
                        None => "noted".into_response(),
                    }
                }))
                .route("/token", post(|State(issuer): State<Shared>, Form(form): Form<HashMap<String, String>>| async move {
                    let request = issuer.authorized.lock().unwrap().last().cloned().unwrap_or_default();
                    match form["grant_type"].as_str() {
                        "authorization_code" if form["code"] == "good-code" => {
                            // PKCE: the verifier must hash to the challenge the browser carried.
                            assert_eq!(b64(Sha256::digest(form["code_verifier"].as_bytes())), request["code_challenge"]);
                            axum::Json(json!({"access_token":"access-1","refresh_token":"refresh-1","id_token":issuer.id_token(&form["client_id"], request.get("nonce").map_or("", String::as_str)),"token_type":"Bearer","expires_in":3600,"scope":"openid offline_access chatgpt.tokens.use.direct"})).into_response()
                        }
                        _ => (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error":"invalid_grant"}))).into_response(),
                    }
                }))
                .route("/v1/models", get(|| async { axum::Json(json!({"models":[{"slug":"fixture-model","display_name":"Fixture Model","visibility":"list"}]})) }))
                .route("/revoke", post(|| async { axum::http::StatusCode::OK }))
                .with_state(issuer);
            axum::serve(listener, router).await.unwrap();
        });
        Running { issuer, task }
    }

    fn id_token(&self, client_id: &str, nonce: &str) -> String {
        let now = chrono::Utc::now().timestamp();
        let header = b64(json!({"alg":"ES256","kid":"k1","typ":"JWT"}).to_string());
        let claims = b64(json!({"iss":self.origin,"aud":client_id,"sub":"user-1","email":"a@example.com","nonce":nonce,"iat":now,"exp":now + 3600}).to_string());
        let signed = format!("{header}.{claims}");
        let signature = self.key.sign(&ring::rand::SystemRandom::new(), signed.as_bytes()).unwrap();
        format!("{signed}.{}", b64(signature.as_ref()))
    }

    pub(crate) fn discovery_url(&self) -> String {
        format!("{}/.well-known/openid-configuration", self.origin)
    }
    pub(crate) fn models_url(&self) -> String {
        format!("{}/v1/models", self.origin)
    }
}
