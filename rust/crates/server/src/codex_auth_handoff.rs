//! What may travel through a ChatGPT sign-in handed to another machine (`packages/shared/src/codexAuthHandoff.ts`,
//! `providerAuthReturnUrl.ts`). The helper that receives a callback only opens OpenAI's authorize endpoint and listens on a loopback port;
//! a code reaches the environment that owns the tokens in a URL fragment, never in a hosted web request.
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
use url::Url;

/// `isLoopbackHost`: the host as `URL.hostname` spells it.
pub fn is_loopback_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "0.0.0.0" | "::1" | "[::1]")
}

/// `providerAuthReturnUrl`: only a local client or the hosted T3 client, never an arbitrary OAuth-supplied URL. Query parameters other than
/// the few a settings page needs are dropped.
pub fn provider_auth_return_url(value: Option<&str>) -> Option<String> {
    static HASH: OnceLock<Regex> = OnceLock::new();
    let mut url = Url::parse(value.filter(|value| !value.is_empty())?).ok()?;
    let desktop = matches!(url.scheme(), "t3code" | "t3code-dev") && url.host_str() == Some("app");
    let web = matches!(url.scheme(), "http" | "https") && (url.host_str().is_some_and(is_loopback_host) || url.origin().ascii_serialization() == "https://app.t3.codes");
    if !url.username().is_empty() || url.password().is_some() || (!desktop && !web) || !(url.path() == "/welcome" || url.path() == "/settings" || url.path().starts_with("/settings/")) {
        return None;
    }
    let path = url.path().to_owned();
    let pairs: Vec<(String, String)> = url.query_pairs().map(|(key, value)| (key.into_owned(), value.into_owned())).collect();
    let keep = |key: &str| path != "/welcome" && ["machine", "project", "checkout", "environmentId", "instanceId"].contains(&key);
    // `searchParams.delete` re-serializes the whole query, whether or not the key was there.
    if pairs.iter().any(|(key, _)| !keep(key)) {
        let kept: Vec<_> = pairs.into_iter().filter(|(key, _)| keep(key)).collect();
        if kept.is_empty() {
            url.set_query(None);
        } else {
            url.query_pairs_mut().clear().extend_pairs(kept);
        }
    }
    let agents = HASH.get_or_init(|| Regex::new(r"^#agents:[\w-]+$").unwrap());
    if path != "/welcome" || !url.fragment().is_some_and(|fragment| agents.is_match(&format!("#{fragment}"))) {
        url.set_fragment(None);
    }
    Some(url.to_string())
}

/// What the helper may open and listen for (`codexAuthorizationRequest`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizationRequest {
    pub authorization_url: String,
    pub redirect_uri: String,
    pub state: String,
}

const INVALID_REQUEST: &str = "Invalid ChatGPT sign-in request.";

/// `codexAuthorizationRequest`: OpenAI's authorize endpoint with a loopback redirect, the S256 challenge of the sign-in and one of the
/// two client kinds; anything else is refused.
pub fn codex_authorization_request(value: &str) -> Result<AuthorizationRequest, &'static str> {
    static REDIRECT: OnceLock<Regex> = OnceLock::new();
    static STATE: OnceLock<Regex> = OnceLock::new();
    static CHALLENGE: OnceLock<Regex> = OnceLock::new();
    static CLIENT: OnceLock<Regex> = OnceLock::new();
    let url = Url::parse(value).map_err(|_| INVALID_REQUEST)?;
    if value.encode_utf16().count() > 16_384 || url.origin().ascii_serialization() != "https://auth.openai.com" || url.path() != "/api/accounts/authorize" || !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err(INVALID_REQUEST);
    }
    let single = |key: &str| -> Result<String, &'static str> {
        let values: Vec<_> = url.query_pairs().filter(|(name, _)| name == key).map(|(_, value)| value.into_owned()).collect();
        match values.as_slice() {
            [value] if !value.is_empty() => Ok(value.clone()),
            _ => Err(INVALID_REQUEST),
        }
    };
    let redirect_uri = single("redirect_uri")?;
    let redirect = Url::parse(&redirect_uri).map_err(|_| INVALID_REQUEST)?;
    let state = single("state")?;
    let valid = REDIRECT.get_or_init(|| Regex::new(r"^http://127\.0\.0\.1:[1-9]\d{0,4}/auth/callback$").unwrap()).is_match(&redirect_uri)
        && redirect.port().is_some()
        && STATE.get_or_init(|| Regex::new(r"^[\w-]{16,128}$").unwrap()).is_match(&state)
        && single("response_type")? == "code"
        && single("code_challenge_method")? == "S256"
        && CHALLENGE.get_or_init(|| Regex::new(r"^[\w-]{43}$").unwrap()).is_match(&single("code_challenge")?)
        && CLIENT.get_or_init(|| Regex::new(r"^(dynamic_agent_client|oaiapp_[\w-]+)$").unwrap()).is_match(&single("client_id")?);
    // A port past 65535 does not parse as one, which `redirect.port().is_some()` already refused.
    if !valid {
        return Err(INVALID_REQUEST);
    }
    Ok(AuthorizationRequest { authorization_url: url.to_string(), redirect_uri, state })
}

/// `codexCallbackUrl`: the callback is the redirect it was registered for, carries this sign-in's state and exactly one of a code or an
/// error. Shared by the helper that listens and the environment that receives the code.
pub fn codex_callback_url(value: &str, redirect_uri: &str, state: &str) -> Result<Url, &'static str> {
    static CLIENT: OnceLock<Regex> = OnceLock::new();
    const MISMATCH: &str = "This redirect URL does not belong to the current sign-in.";
    let callback = Url::parse(value).map_err(|_| MISMATCH)?;
    let expected = Url::parse(redirect_uri).map_err(|_| MISMATCH)?;
    let all = |key: &str| -> Vec<String> { callback.query_pairs().filter(|(name, _)| name == key).map(|(_, value)| value.into_owned()).collect() };
    let (states, codes, errors, clients) = (all("state"), all("code"), all("error"), all("client_id"));
    let client = CLIENT.get_or_init(|| Regex::new(r"^oaiapp_[\w-]+$").unwrap());
    let valid = value.encode_utf16().count() <= 16_384
        && callback.origin() == expected.origin()
        && callback.path() == expected.path()
        && callback.username().is_empty()
        && callback.password().is_none()
        && callback.fragment().is_none()
        && states.len() == 1
        && states[0] == state
        && clients.len() <= 1
        && clients.first().is_none_or(|id| client.is_match(id))
        && ((codes.len() == 1 && !codes[0].is_empty() && errors.is_empty()) || (errors.len() == 1 && !errors[0].is_empty() && codes.is_empty()));
    if valid { Ok(callback) } else { Err(MISMATCH) }
}

/// The sign-in a desktop helper is asked to open (`CodexAuthHandoff`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Handoff {
    pub authorization_url: String,
    pub return_url: String,
    pub environment_id: String,
    pub instance_id: String,
    pub flow_id: String,
}

impl Handoff {
    /// The schema's bounds (`CodexAuthHandoff`): lengths in UTF-16 units.
    fn within_bounds(&self) -> bool {
        let units = |text: &str| text.encode_utf16().count();
        units(&self.authorization_url) <= 16_384 && units(&self.return_url) <= 4_096 && (1..=128).contains(&units(&self.flow_id)) && !self.environment_id.is_empty() && !self.instance_id.is_empty()
    }
}

/// `codexAuthHandoffUrl`: `t3code://auth/codex?request=<json>` (`t3code-dev` in development). `None` when the handoff is out of bounds.
pub fn codex_auth_handoff_url(input: &Handoff, development: bool) -> Option<String> {
    if !input.within_bounds() {
        return None;
    }
    let mut url = Url::parse(&format!("{}://auth/codex", if development { "t3code-dev" } else { "t3code" })).ok()?;
    url.query_pairs_mut().append_pair("request", &serde_json::to_string(input).ok()?);
    Some(url.to_string())
}

/// `readCodexAuthHandoff`: the handoff a link carries, if the link is exactly that and its two URLs pass their checks.
pub fn read_codex_auth_handoff(value: &str, development: bool) -> Option<Handoff> {
    let url = Url::parse(value).ok()?;
    let requests: Vec<_> = url.query_pairs().filter(|(key, _)| key == "request").map(|(_, value)| value.into_owned()).collect();
    if value.encode_utf16().count() > 32_768 || url.scheme() != if development { "t3code-dev" } else { "t3code" } || url.host_str() != Some("auth") || url.path() != "/codex" || !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() || requests.len() != 1 {
        return None;
    }
    let input: Handoff = serde_json::from_str(&requests[0]).ok()?;
    if !input.within_bounds() {
        return None;
    }
    codex_authorization_request(&input.authorization_url).ok()?;
    provider_auth_return_url(Some(&input.return_url))?;
    Some(input)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Delivery {
    environment_id: String,
    instance_id: String,
    flow_id: String,
    callback_url: String,
    return_hash: String,
}

/// What a delivery URL carries once read (`readCodexAuthDelivery`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadDelivery {
    pub environment_id: String,
    pub instance_id: String,
    pub flow_id: String,
    pub callback_url: String,
    pub return_hash: String,
    pub return_url: String,
}

/// `codexAuthDeliveryUrl`: the callback goes back to the return address in its fragment, so it reaches the client and no server in between.
pub fn codex_auth_delivery_url(input: &Handoff, callback_url: &str) -> Result<String, &'static str> {
    let request = codex_authorization_request(&input.authorization_url)?;
    codex_callback_url(callback_url, &request.redirect_uri, &request.state)?;
    let destination = provider_auth_return_url(Some(&input.return_url)).ok_or("Invalid T3 Code return address.")?;
    let mut url = Url::parse(&destination).map_err(|_| "Invalid T3 Code return address.")?;
    let return_hash = url.fragment().map(|fragment| format!("#{fragment}")).unwrap_or_default();
    let delivery = Delivery { environment_id: input.environment_id.clone(), instance_id: input.instance_id.clone(), flow_id: input.flow_id.clone(), callback_url: callback_url.to_owned(), return_hash };
    let encoded = url::form_urlencoded::byte_serialize(serde_json::to_string(&delivery).map_err(|_| "Invalid T3 Code return address.")?.as_bytes()).collect::<String>().replace('+', "%20");
    url.set_fragment(Some(&format!("codex-auth={encoded}")));
    Ok(url.to_string())
}

/// `readCodexAuthDelivery`.
pub fn read_codex_auth_delivery(value: &str) -> Option<ReadDelivery> {
    let url = Url::parse(value).ok()?;
    let fragment = format!("#{}", url.fragment()?);
    let body = fragment.strip_prefix("#codex-auth=")?;
    if fragment.encode_utf16().count() > 32_768 {
        return None;
    }
    let decoded = percent_decode(body)?;
    let delivery: Delivery = serde_json::from_str(&decoded).ok()?;
    if delivery.flow_id.is_empty() || delivery.flow_id.encode_utf16().count() > 128 || delivery.callback_url.encode_utf16().count() > 16_384 || delivery.return_hash.encode_utf16().count() > 128 {
        return None;
    }
    let destination = provider_auth_return_url(Some(value))?;
    let mut return_url = Url::parse(&destination).ok()?;
    return_url.set_fragment(delivery.return_hash.strip_prefix('#').filter(|hash| !hash.is_empty()));
    let sanitized = provider_auth_return_url(Some(return_url.as_str()))?;
    Some(ReadDelivery { environment_id: delivery.environment_id, instance_id: delivery.instance_id, flow_id: delivery.flow_id, callback_url: delivery.callback_url, return_hash: delivery.return_hash, return_url: sanitized })
}

/// `decodeURIComponent`: percent escapes only (a `+` stays a plus), and a malformed or non-UTF-8 sequence fails.
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let pair = text.get(index + 1..index + 3)?;
            out.push(u8::from_str_radix(pair, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}
