//! ChatGPT sign-in for managed Codex: the checks on the sign-in address and on the redirect that comes back, and the folds
//! of the two streams the flow follows. Port of `packages/shared/src/codexAuthHandoff.ts` (the parts the desktop listener and
//! the page share), `providerAuthReturnUrl.ts`, and the stream handling of `CodexSetupSection.tsx`.
//!
//! The wire shapes are `provider.codex.auth-callback.subscribe` (`{"phase":"ready"}`, then `{"phase":"finished","callbackUrl"}`)
//! and `provider.chatgpt.handoff.subscribe` (`{"phase":"auth","state":ProviderAuthState}` items, then
//! `{"phase":"finished","profile":{registration,credentials}}`). Nothing here contacts OpenAI.
use serde_json::Value;
use url::Url;

const INVALID_REQUEST: &str = "Invalid ChatGPT sign-in request.";
const FOREIGN_REDIRECT: &str = "This redirect URL does not belong to the current sign-in.";

/// What the loopback listener needs from the sign-in address: where OpenAI will redirect and the `state` it must carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizationRequest {
    pub authorization_url: String,
    pub redirect_uri: String,
    pub state: String,
}

fn single(url: &Url, key: &str) -> Result<String, String> {
    let values: Vec<_> = url.query_pairs().filter(|(name, _)| name == key).map(|(_, value)| value.into_owned()).collect();
    match values.as_slice() {
        [value] if !value.is_empty() => Ok(value.clone()),
        _ => Err(INVALID_REQUEST.into()),
    }
}
fn word(value: &str, min: usize, max: usize) -> bool {
    (min..=max).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

/// `codexAuthorizationRequest`: the helper only opens OpenAI's authorize endpoint and receives a loopback callback.
pub fn authorization_request(value: &str) -> Result<AuthorizationRequest, String> {
    let invalid = || INVALID_REQUEST.to_owned();
    let url = Url::parse(value).map_err(|_| invalid())?;
    if value.len() > 16_384
        || url.scheme() != "https"
        || url.host_str() != Some("auth.openai.com")
        || url.port().is_some()
        || url.path() != "/api/accounts/authorize"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid());
    }
    let redirect_uri = single(&url, "redirect_uri")?;
    let state = single(&url, "state")?;
    let port = redirect_uri.strip_prefix("http://127.0.0.1:").and_then(|rest| rest.strip_suffix("/auth/callback"));
    let port_ok = port.is_some_and(|port| !port.starts_with('0') && !port.is_empty() && port.len() <= 5 && port.bytes().all(|byte| byte.is_ascii_digit()) && port.parse::<u32>().is_ok_and(|port| port <= 65_535));
    let client = single(&url, "client_id")?;
    let client_ok = client == "dynamic_agent_client" || client.strip_prefix("oaiapp_").is_some_and(|rest| !rest.is_empty() && word(rest, 1, usize::MAX));
    let challenge = single(&url, "code_challenge")?;
    if !port_ok
        || !word(&state, 16, 128)
        || single(&url, "response_type")? != "code"
        || single(&url, "code_challenge_method")? != "S256"
        || !(challenge.len() == 43 && word(&challenge, 43, 43))
        || !client_ok
    {
        return Err(invalid());
    }
    Ok(AuthorizationRequest { authorization_url: url.to_string(), redirect_uri, state })
}

/// `codexCallbackUrl`: shared by the desktop listener and the environment that receives the code. The redirect must be the
/// expected loopback address with this sign-in's `state`, and carry exactly one `code` or exactly one `error`.
pub fn callback_url(value: &str, redirect_uri: &str, state: &str) -> Result<Url, String> {
    let foreign = || FOREIGN_REDIRECT.to_owned();
    let callback = Url::parse(value).map_err(|_| foreign())?;
    let expected = Url::parse(redirect_uri).map_err(|_| foreign())?;
    let all = |key: &str| -> Vec<String> { callback.query_pairs().filter(|(name, _)| name == key).map(|(_, value)| value.into_owned()).collect() };
    let (states, codes, errors, clients) = (all("state"), all("code"), all("error"), all("client_id"));
    let client_ok = match clients.as_slice() {
        [] => true,
        [client] => client.strip_prefix("oaiapp_").is_some_and(|rest| !rest.is_empty() && word(rest, 1, usize::MAX)),
        _ => false,
    };
    let outcome_ok = (codes.len() == 1 && !codes[0].is_empty() && errors.is_empty()) || (errors.len() == 1 && !errors[0].is_empty() && codes.is_empty());
    if value.len() > 16_384
        || callback.origin() != expected.origin()
        || callback.path() != expected.path()
        || !callback.username().is_empty()
        || callback.password().is_some()
        || callback.fragment().is_some()
        || states.len() != 1
        || states[0] != state
        || !client_ok
        || !outcome_ok
    {
        return Err(foreign());
    }
    Ok(callback)
}

/// `providerAuthReturnUrl`: sign-in returns only to a local client or the hosted T3 client, never to an address the OAuth
/// provider supplied. Returns the address without the query it may not carry.
pub fn return_url(value: &str) -> Option<String> {
    let mut url = Url::parse(value).ok()?;
    let desktop = matches!(url.scheme(), "t3code" | "t3code-dev") && url.host_str() == Some("app");
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    let web = matches!(url.scheme(), "http" | "https") && (loopback || (url.scheme() == "https" && url.host_str() == Some("app.t3.codes") && url.port().is_none()));
    let welcome = url.path() == "/welcome";
    if !url.username().is_empty() || url.password().is_some() || (!desktop && !web) || (!welcome && url.path() != "/settings" && !url.path().starts_with("/settings/")) {
        return None;
    }
    let kept: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(key, _)| !welcome && matches!(key.as_ref(), "machine" | "project" | "checkout" | "environmentId" | "instanceId"))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    url.set_query(None);
    if !kept.is_empty() {
        url.query_pairs_mut().extend_pairs(kept);
    }
    let agents = url.fragment().is_some_and(|fragment| fragment.strip_prefix("agents:").is_some_and(|id| !id.is_empty() && word(id, 1, usize::MAX)));
    if !welcome || !agents {
        url.set_fragment(None);
    }
    Some(url.to_string())
}

// ---- the streams -----------------------------------------------------------------------------

/// Where the browser sign-in stands, from `provider.codex.auth-callback.subscribe`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Callback {
    /// Nothing has been heard yet.
    #[default]
    Waiting,
    /// The environment is listening for the redirect.
    Listening,
    /// The redirect arrived; this is its address.
    Received(String),
}
impl Callback {
    /// Folds one stream item. An item that is not a known phase leaves the state as it was (a newer server may send more).
    pub fn apply(self, item: &Value) -> Self {
        match (item["phase"].as_str(), item["callbackUrl"].as_str()) {
            (Some("ready"), _) if !matches!(self, Callback::Received(_)) => Callback::Listening,
            (Some("finished"), Some(url)) if !url.is_empty() => Callback::Received(url.to_owned()),
            _ => self,
        }
    }
}

/// What a sign-in on another computer's behalf has done, from `provider.chatgpt.handoff.subscribe`.
#[derive(Debug, Clone, PartialEq)]
pub enum Handoff {
    Waiting,
    /// The sign-in is under way; the text is the state's own message, if it has one.
    Signing { phase: String, message: Option<String> },
    /// The sign-in failed or was cancelled; the person is told why.
    Failed(String),
    /// The sign-in is done; the profile is to be imported into the environment that asked.
    Finished { profile: Value },
}
const INTERRUPTED: &str = "ChatGPT sign-in on the primary environment was interrupted. Try again.";
const COULD_NOT_FINISH: &str = "ChatGPT sign-in could not finish. Try again.";

impl Handoff {
    pub fn apply(self, item: &Value) -> Self {
        match item["phase"].as_str() {
            Some("finished") if item["profile"]["registration"].is_object() && item["profile"]["credentials"].is_object() => Handoff::Finished { profile: item["profile"].clone() },
            Some("auth") => {
                let state = &item["state"];
                let phase = state["phase"].as_str().unwrap_or_default().to_owned();
                let message = state["message"].as_str().map(str::to_owned);
                if matches!(phase.as_str(), "failed" | "cancelled") {
                    Handoff::Failed(message.unwrap_or_else(|| COULD_NOT_FINISH.into()))
                } else {
                    Handoff::Signing { phase, message }
                }
            }
            _ => self,
        }
    }
    /// The stream ended or errored before a profile arrived.
    pub fn interrupted(self) -> Self {
        match self {
            Handoff::Finished { .. } | Handoff::Failed(_) => self,
            _ => Handoff::Failed(INTERRUPTED.into()),
        }
    }
}

/// `primaryAuthEnvironmentId`: a sign-in for a remote environment is done on this computer's own server, when the app has one
/// connected on loopback and the page runs on this computer. Without one the remote environment signs in by itself.
pub fn handoff_through_local_server(environment_is_loopback: bool, local_server_connected: bool, page_on_this_computer: bool) -> bool {
    !environment_is_loopback && local_server_connected && page_on_this_computer
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
    const STATE: &str = "0123456789abcdef0123456789abcdef";
    fn address(redirect: &str, extra: &str) -> String {
        format!("https://auth.openai.com/api/accounts/authorize?response_type=code&client_id=dynamic_agent_client&redirect_uri={redirect}&state={STATE}&code_challenge={CHALLENGE}&code_challenge_method=S256{extra}")
    }
    const REDIRECT: &str = "http%3A%2F%2F127.0.0.1%3A1455%2Fauth%2Fcallback";

    #[test]
    fn only_openais_authorize_endpoint_with_a_loopback_redirect_is_opened() {
        let request = authorization_request(&address(REDIRECT, "")).unwrap();
        assert_eq!((request.redirect_uri.as_str(), request.state.as_str()), ("http://127.0.0.1:1455/auth/callback", STATE));
        for hostile in [
            address(REDIRECT, "").replace("auth.openai.com", "auth.openai.com.evil.test"),
            address(REDIRECT, "").replace("https://", "http://"),
            address(REDIRECT, "").replace("/api/accounts/authorize", "/api/accounts/other"),
            address(REDIRECT, "").replace("https://", "https://user:pass@"),
            address(REDIRECT, "#frag"),
            address("http%3A%2F%2Fevil.test%2Fauth%2Fcallback", ""),
            address("http%3A%2F%2F127.0.0.1%3A0%2Fauth%2Fcallback", ""),
            address("http%3A%2F%2F127.0.0.1%3A70000%2Fauth%2Fcallback", ""),
            address("http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback", ""),
            address(REDIRECT, &format!("&state={STATE}")),
            address(REDIRECT, "&redirect_uri=http%3A%2F%2F127.0.0.1%3A1456%2Fauth%2Fcallback"),
            address(REDIRECT, "").replace("response_type=code", "response_type=token"),
            address(REDIRECT, "").replace("S256", "plain"),
            address(REDIRECT, "").replace(STATE, "short"),
            address(REDIRECT, "").replace("dynamic_agent_client", "someone_else"),
            "not a url".to_owned(),
        ] {
            assert_eq!(authorization_request(&hostile), Err(INVALID_REQUEST.to_owned()), "{hostile}");
        }
        assert!(authorization_request(&address(REDIRECT, "").replace("dynamic_agent_client", "oaiapp_abc-1")).is_ok());
    }

    #[test]
    fn a_redirect_is_taken_only_for_its_own_sign_in_and_only_with_one_outcome() {
        let redirect = "http://127.0.0.1:1455/auth/callback";
        let ok = |query: &str| callback_url(&format!("{redirect}?{query}"), redirect, STATE);
        assert!(ok(&format!("code=abc&state={STATE}")).is_ok());
        assert!(ok(&format!("error=access_denied&state={STATE}")).is_ok(), "a refusal is delivered too, so the environment can say so");
        assert!(ok(&format!("code=abc&state={STATE}&client_id=oaiapp_x1")).is_ok());
        for bad in [
            format!("code=abc&state=other"),
            format!("code=abc"),
            format!("state={STATE}"),
            format!("code=a&code=b&state={STATE}"),
            format!("code=abc&error=x&state={STATE}"),
            format!("code=&state={STATE}"),
            format!("code=abc&state={STATE}&state={STATE}"),
            format!("code=abc&state={STATE}&client_id=evil"),
            format!("code=abc&state={STATE}&client_id=oaiapp_a&client_id=oaiapp_b"),
        ] {
            assert_eq!(ok(&bad), Err(FOREIGN_REDIRECT.to_owned()), "{bad}");
        }
        for elsewhere in ["http://127.0.0.1:1456/auth/callback", "http://localhost:1455/auth/callback", "http://127.0.0.1:1455/other", "https://127.0.0.1:1455/auth/callback", "http://u:p@127.0.0.1:1455/auth/callback"] {
            assert!(callback_url(&format!("{elsewhere}?code=abc&state={STATE}"), redirect, STATE).is_err(), "{elsewhere}");
        }
        assert!(callback_url(&format!("{redirect}?code=abc&state={STATE}#x"), redirect, STATE).is_err());
        assert!(callback_url("nonsense", redirect, STATE).is_err());
    }

    #[test]
    fn sign_in_returns_only_to_this_client_or_the_hosted_one_with_a_trimmed_query() {
        assert_eq!(return_url("http://localhost:5733/welcome?x=1#agents:env-1").as_deref(), Some("http://localhost:5733/welcome#agents:env-1"));
        assert_eq!(return_url("http://127.0.0.1:3773/settings/providers?instanceId=codex&evil=1#frag").as_deref(), Some("http://127.0.0.1:3773/settings/providers?instanceId=codex"));
        assert_eq!(return_url("https://app.t3.codes/settings/providers").as_deref(), Some("https://app.t3.codes/settings/providers"));
        assert_eq!(return_url("t3code://app/settings/general").as_deref(), Some("t3code://app/settings/general"));
        for refused in ["https://evil.test/settings/providers", "https://app.t3.codes.evil.test/settings", "http://app.t3.codes/settings", "http://localhost/other", "http://u:p@localhost/settings", "javascript:alert(1)", "t3code://elsewhere/settings", ""] {
            assert_eq!(return_url(refused), None, "{refused}");
        }
    }

    #[test]
    fn the_callback_stream_goes_from_nothing_to_listening_to_received_and_ignores_what_it_does_not_know() {
        let mut state = Callback::default();
        state = state.apply(&json!({"phase":"future-phase"}));
        assert_eq!(state, Callback::Waiting);
        state = state.apply(&json!({"phase":"ready"}));
        assert_eq!(state, Callback::Listening);
        state = state.apply(&json!({"phase":"finished","callbackUrl":"http://127.0.0.1:1455/auth/callback?code=c&state=s"}));
        assert_eq!(state, Callback::Received("http://127.0.0.1:1455/auth/callback?code=c&state=s".into()));
        assert_eq!(state.clone().apply(&json!({"phase":"ready"})), state, "a late ready does not undo it");
        assert_eq!(Callback::Listening.apply(&json!({"phase":"finished"})), Callback::Listening, "a finish without an address is not one");
    }

    #[test]
    fn the_handoff_stream_reports_progress_ends_with_a_profile_and_says_why_a_sign_in_failed() {
        let profile = json!({"registration":{"clientId":"c"},"credentials":{"clientId":"c","accessToken":"a"}});
        let mut state = Handoff::Waiting;
        state = state.apply(&json!({"phase":"auth","state":{"phase":"waiting","message":"Finish signing in in your browser."}}));
        assert_eq!(state, Handoff::Signing { phase: "waiting".into(), message: Some("Finish signing in in your browser.".into()) });
        state = state.apply(&json!({"phase":"finished","profile":profile}));
        assert_eq!(state, Handoff::Finished { profile: profile.clone() });
        assert_eq!(state.clone().interrupted(), state, "a finished sign-in is not undone by the stream closing");
        assert_eq!(Handoff::Waiting.apply(&json!({"phase":"auth","state":{"phase":"failed","message":"Account mismatch."}})), Handoff::Failed("Account mismatch.".into()));
        assert_eq!(Handoff::Waiting.apply(&json!({"phase":"auth","state":{"phase":"cancelled"}})), Handoff::Failed(COULD_NOT_FINISH.into()));
        assert_eq!(Handoff::Signing { phase: "waiting".into(), message: None }.interrupted(), Handoff::Failed(INTERRUPTED.into()));
        assert_eq!(Handoff::Waiting.apply(&json!({"phase":"finished","profile":{}})), Handoff::Waiting, "a profile without its parts is not a profile");
    }

    #[test]
    fn a_remote_environment_signs_in_through_this_computers_own_server_when_it_has_one() {
        assert!(handoff_through_local_server(false, true, true));
        assert!(!handoff_through_local_server(true, true, true), "an environment on this computer signs in by itself");
        assert!(!handoff_through_local_server(false, false, true));
        assert!(!handoff_through_local_server(false, true, false));
    }
}
