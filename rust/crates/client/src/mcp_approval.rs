//! The agent sign-in page's decisions (`ConnectAgentSurface.tsx`): which access the person may hand the agent, whether this
//! browser's session can approve it in one click, the decision the server is sent, and how the server's answers read. The page
//! itself (`connect_agent.rs` of the UI) asks the server's `/oauth/mcp/approval` and `/oauth/mcp/decision`.
use serde_json::{Map, Value};
use t3_contracts::{AuthMcpApprovalDetails, AuthMcpClientAccess};

/// What the page says when the server cannot be reached.
pub const UNREACHABLE: &str = "Could not reach this environment. Try again.";
/// What it says when a refused decision has no message of its own.
pub const CANNOT_CONTINUE: &str = "The sign-in could not continue.";

/// `AuthMcpClientAccess.literals`, in the order the page lists them, with `accessConfig`'s label and description
/// (`runtimeModeConfig` for the four modes).
pub const OPTIONS: [(AuthMcpClientAccess, &str, &str); 5] = [
    (AuthMcpClientAccess::ReadOnly, "Read only", "Read projects and threads. Cannot start, message or change anything."),
    (AuthMcpClientAccess::ApprovalRequired, "Supervised", "Ask before commands and file changes."),
    (AuthMcpClientAccess::AutoAcceptEdits, "Auto-accept edits", "Auto-approve edits, ask before other actions."),
    (AuthMcpClientAccess::Auto, "Auto", "Supported providers approve routine actions; others still ask."),
    (AuthMcpClientAccess::FullAccess, "Full access", "Allow commands and edits without prompts."),
];

/// The wire name of an access, which is also what the page's radio buttons carry.
pub fn access_name(access: AuthMcpClientAccess) -> &'static str {
    match access {
        AuthMcpClientAccess::ReadOnly => "read-only",
        AuthMcpClientAccess::ApprovalRequired => "approval-required",
        AuthMcpClientAccess::AutoAcceptEdits => "auto-accept-edits",
        AuthMcpClientAccess::Auto => "auto",
        AuthMcpClientAccess::FullAccess => "full-access",
    }
}

/// `redirectsToThisComputer`: a CLI agent's callback is a loopback address; a hosted agent's is its own server.
pub fn redirects_to_this_computer(redirect_host: &str) -> bool {
    let host = redirect_host.trim().to_lowercase();
    let name = match host.strip_prefix('[') {
        Some(rest) => rest.split_once(']').map_or(host.as_str(), |(inner, _)| &host[..inner.len() + 2]),
        None => host.split(':').next().unwrap_or_default(),
    };
    matches!(name, "localhost" | "127.0.0.1" | "0.0.0.0" | "::1" | "[::1]")
}

/// `oneClickApproves`: this browser's session may only grant access whose scopes it holds itself; the server lists those.
pub fn one_click_approves(details: &AuthMcpApprovalDetails, access: AuthMcpClientAccess) -> bool {
    details.csrf_token.is_some() && details.one_click_access.as_ref().is_some_and(|allowed| allowed.contains(&access))
}

/// What Approve sends: one click when the session can, else the pairing code typed.
pub fn approve_decision(details: &AuthMcpApprovalDetails, access: AuthMcpClientAccess, pairing_code: &str) -> Value {
    match details.csrf_token.as_ref().filter(|_| one_click_approves(details, access)) {
        Some(csrf_token) => serde_json::json!({"_tag":"browser-session","access":access_name(access),"csrfToken":csrf_token}),
        None => serde_json::json!({"_tag":"pairing-code","access":access_name(access),"code":pairing_code.trim()}),
    }
}

pub fn deny_decision() -> Value {
    serde_json::json!({"_tag":"deny"})
}

/// Whether Approve may be pressed: nothing is in flight and either one click or a code is there to approve with.
pub fn can_approve(pending: bool, one_click: bool, pairing_code: &str) -> bool {
    !pending && (one_click || !pairing_code.trim().is_empty())
}

/// `readAuthorizationRequest`: the page's query as an object (`Object.fromEntries(searchParams)`: a name given twice keeps its last).
pub fn authorization_from_query(query: &str) -> Value {
    let mut fields = Map::new();
    for (name, value) in url::form_urlencoded::parse(query.trim_start_matches('?').as_bytes()) {
        fields.insert(name.into_owned(), Value::String(value.into_owned()));
    }
    Value::Object(fields)
}

/// What the server answered to a call of the page.
#[derive(Debug, Clone, PartialEq)]
pub enum Answer<T> {
    Ok(T),
    /// The browser must follow this: an approval, a denial, or a protocol error the agent should receive.
    Redirect(String),
    Error(String),
}

fn read<T>(status: u16, body: &Value, parse: impl FnOnce(&Value) -> Option<T>) -> Answer<T> {
    if let Some(url) = body.get("redirectTo").and_then(Value::as_str) {
        return Answer::Redirect(url.to_owned());
    }
    if (200..300).contains(&status) {
        if let Some(value) = parse(body) {
            return Answer::Ok(value);
        }
        return Answer::Error(UNREACHABLE.into());
    }
    // `isApprovalError`: only the server's own approval error carries a message to show.
    match (body.get("_tag").and_then(Value::as_str), body.get("message").and_then(Value::as_str)) {
        (Some("AuthMcpApprovalError"), Some(message)) => Answer::Error(message.to_owned()),
        _ => Answer::Error(UNREACHABLE.into()),
    }
}

/// The answer to `POST /oauth/mcp/approval`.
pub fn read_details(status: u16, body: &Value) -> Answer<AuthMcpApprovalDetails> {
    read(status, body, |body| serde_json::from_value(body.clone()).ok())
}

/// The answer to `POST /oauth/mcp/decision`: a redirect, or a message.
pub fn read_decision(status: u16, body: &Value) -> Answer<()> {
    read(status, body, |_| Some(()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn details(csrf: Option<&str>, one_click: Option<&[AuthMcpClientAccess]>) -> AuthMcpApprovalDetails {
        AuthMcpApprovalDetails { client_name: "Claude Code".into(), redirect_host: "localhost:51234".into(), environment_host: "h".into(), csrf_token: csrf.map(Into::into), one_click_access: one_click.map(<[_]>::to_vec) }
    }

    #[test]
    fn a_loopback_callback_returns_to_this_computer_and_any_other_goes_to_whoever_runs_it() {
        for local in ["localhost:51234", "127.0.0.1:9", "LOCALHOST", "[::1]:8080", "0.0.0.0:1"] {
            assert!(redirects_to_this_computer(local), "{local}");
        }
        for remote in ["claude.ai", "agent.example.com:443", "127.0.0.2:1", "localhost.evil.test"] {
            assert!(!redirects_to_this_computer(remote), "{remote}");
        }
    }

    #[test]
    fn one_click_needs_a_session_that_holds_the_access_and_otherwise_the_code_decides() {
        let read_only = [AuthMcpClientAccess::ReadOnly];
        let reader = details(Some("csrf"), Some(&read_only));
        assert!(one_click_approves(&reader, AuthMcpClientAccess::ReadOnly));
        assert!(!one_click_approves(&reader, AuthMcpClientAccess::Auto), "a session cannot grant what it does not hold");
        assert!(!one_click_approves(&details(None, Some(&read_only)), AuthMcpClientAccess::ReadOnly), "no token, no click");
        assert!(!one_click_approves(&details(Some("csrf"), None), AuthMcpClientAccess::ReadOnly));
        assert_eq!(approve_decision(&reader, AuthMcpClientAccess::ReadOnly, " ABC "), json!({"_tag":"browser-session","access":"read-only","csrfToken":"csrf"}));
        assert_eq!(approve_decision(&reader, AuthMcpClientAccess::Auto, " ABC "), json!({"_tag":"pairing-code","access":"auto","code":"ABC"}));
        assert_eq!(deny_decision(), json!({"_tag":"deny"}));
        assert!(can_approve(false, true, "") && can_approve(false, false, "x") && !can_approve(false, false, "  ") && !can_approve(true, true, "x"));
    }

    #[test]
    fn the_query_becomes_the_object_the_server_checks_and_the_last_of_a_repeated_name_wins() {
        assert_eq!(authorization_from_query("?client_id=a%20b&state=s1&state=s2&empty="), json!({"client_id":"a b","state":"s2","empty":""}));
        assert_eq!(authorization_from_query(""), json!({}));
    }

    #[test]
    fn the_servers_answers_read_as_details_a_redirect_or_a_message() {
        let body = json!({"clientName":"Claude Code","redirectHost":"localhost:51234","environmentHost":"h","csrfToken":"t","oneClickAccess":["read-only"]});
        assert_eq!(read_details(200, &body), Answer::Ok(details(Some("t"), Some(&[AuthMcpClientAccess::ReadOnly]))));
        assert_eq!(read_details(200, &json!({"redirectTo":"http://localhost:1/cb?error=invalid_request"})), Answer::Redirect("http://localhost:1/cb?error=invalid_request".into()));
        assert_eq!(read_details(400, &json!({"_tag":"AuthMcpApprovalError","message":"Unknown client."})), Answer::Error("Unknown client.".into()));
        assert_eq!(read_details(500, &json!({"message":"boom"})), Answer::Error(UNREACHABLE.into()));
        assert_eq!(read_details(200, &json!({"nonsense":true})), Answer::Error(UNREACHABLE.into()));
        assert_eq!(read_decision(200, &json!({"redirectTo":"http://localhost:1/cb?code=x"})), Answer::Redirect("http://localhost:1/cb?code=x".into()));
        assert_eq!(read_decision(400, &json!({"_tag":"AuthMcpApprovalError","message":"Enter a pairing code instead."})), Answer::Error("Enter a pairing code instead.".into()));
    }
}
