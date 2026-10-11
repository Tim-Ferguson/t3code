//! Who can reach this server: the permission choices of a new pairing link, how links and signed-in clients are
//! ordered and worded. Port of the pure parts of `ConnectionsSettings.tsx` and `ConnectionsSettings.logic.ts`
//! (`PAIRING_SCOPE_OPTIONS`, `togglePairingScopeSelection`, `canRevokeOtherClients`, `sortDesktopPairingLinks`,
//! `sortDesktopClientSessions`, `summarizeAuthorizedClients`) and `formatExpiresInLabel` of `timestampFormat.ts`.
use serde_json::Value;
use t3_contracts::AuthEnvironmentScope::{self, *};

pub const PAIRING_LINKS: &str = "/api/auth/pairing-links";
pub const PAIRING_LINK_REVOKE: &str = "/api/auth/pairing-links/revoke";
pub const CLIENTS: &str = "/api/auth/clients";
pub const CLIENT_REVOKE: &str = "/api/auth/clients/revoke";
pub const CLIENTS_REVOKE_OTHERS: &str = "/api/auth/clients/revoke-others";
pub const PAIRING_TOKEN: &str = "/api/auth/pairing-token";

/// The permissions a new link can carry, in the original's order, with what each lets the client do.
pub const SCOPE_OPTIONS: [(AuthEnvironmentScope, &str, &str); 16] = [
    (OrchestrationRead, "View environment", "Read threads, status, checkpoints, and configuration."),
    (OrchestrationOperate, "Operate tasks", "Start, update, and stop tasks."),
    (SettingsWrite, "Change environment settings", "Edit environment preferences and keybindings."),
    (ProvidersManage, "Manage providers", "Configure, install, sign in to, and update providers and usage sources."),
    (EnvironmentMaintain, "Maintain environment", "Update the server and control environment processes."),
    (PreviewOperate, "Control previews", "Open browser previews and host browser automation."),
    (DiagnosticsRead, "View diagnostics and usage", "Read process diagnostics, resource history, and usage totals."),
    (TerminalRead, "View terminals", "Read existing terminal output and status."),
    (TerminalOperate, "Use terminals", "Create terminals and send input to running shells."),
    (SourceControlWrite, "Change source control", "Commit, push, manage branches and repositories, and change pull requests."),
    (FilesystemRead, "Read files", "Browse host files, search workspaces, and inspect local changes."),
    (FilesystemWrite, "Write files", "Edit workspace files and save plans to disk."),
    (AccessRead, "View access", "Inspect pairing links and authorized clients."),
    (AccessWrite, "Manage access", "Issue and revoke credentials for other clients."),
    (RelayRead, "View relay", "Inspect managed relay connectivity."),
    (RelayWrite, "Manage relay", "Change managed tunnel connectivity."),
];

pub const READ_ONLY: [AuthEnvironmentScope; 4] = [OrchestrationRead, FilesystemRead, DiagnosticsRead, TerminalRead];

pub fn wire(scope: AuthEnvironmentScope) -> String {
    serde_json::to_value(scope).ok().and_then(|value| value.as_str().map(str::to_owned)).unwrap_or_default()
}

/// What a click on one permission does: using terminals needs viewing them, and no longer viewing them ends using them.
pub fn toggle(current: &[AuthEnvironmentScope], scope: AuthEnvironmentScope, checked: bool) -> Vec<AuthEnvironmentScope> {
    let without: Vec<_> = current.iter().copied().filter(|candidate| *candidate != scope).collect();
    if !checked {
        return if scope == TerminalRead { without.into_iter().filter(|candidate| *candidate != TerminalOperate).collect() } else { without };
    }
    let mut next = without;
    next.push(scope);
    if scope == TerminalOperate && !next.contains(&TerminalRead) {
        next.push(TerminalRead);
    }
    next
}

/// The standard permissions the connection is allowed to hand on.
pub fn standard(delegatable: &[AuthEnvironmentScope]) -> Vec<AuthEnvironmentScope> {
    t3_contracts::AUTH_STANDARD_CLIENT_SCOPES.iter().copied().filter(|scope| delegatable.contains(scope)).collect()
}

/// The link to open on the device being paired: this server's `/pair` page with the credential in the fragment, which
/// a server never sees in a request.
pub fn pairing_url(origin: &str, credential: &str) -> String {
    format!("{}/pair#token={credential}", origin.trim_end_matches('/'))
}

/// "Expires in 4m 12s", with second precision under an hour (`formatExpiresInLabel`).
pub fn expires_in(expires_at_ms: i64, now_ms: i64) -> String {
    let difference = expires_at_ms - now_ms;
    if difference <= 0 {
        return "Expired".into();
    }
    let total = difference / 1000;
    if total < 5 {
        return "Expires in a moment".into();
    }
    if total < 60 {
        return format!("Expires in {total}s");
    }
    if total < 3600 {
        let (minutes, seconds) = (total / 60, total % 60);
        return if seconds == 0 { format!("Expires in {minutes}m") } else { format!("Expires in {minutes}m {seconds}s") };
    }
    let part = |value: i64, unit: &str| (value > 0).then(|| format!("{value}{unit}"));
    let join = |parts: Vec<Option<String>>| parts.into_iter().flatten().collect::<Vec<_>>().join(" ");
    if total < 86_400 {
        return format!("Expires in {}", join(vec![part(total / 3600, "h"), part(total % 3600 / 60, "m"), part(total % 60, "s")]));
    }
    let remainder = total % 86_400;
    format!("Expires in {}", join(vec![part(total / 86_400, "d"), part(remainder / 3600, "h"), part(remainder % 3600 / 60, "m"), part(remainder % 60, "s")]))
}

fn instant(value: &Value, key: &str) -> i64 {
    value[key].as_str().and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok()).map_or(0, |time| time.timestamp_millis())
}

pub fn expires_at_ms(link: &Value) -> i64 {
    instant(link, "expiresAt")
}

/// Newest first.
pub fn sort_links(links: &mut [Value]) {
    links.sort_by_key(|link| std::cmp::Reverse(instant(link, "createdAt")));
}

/// This device first, then the connected, then the most recently issued.
pub fn sort_clients(sessions: &mut [Value]) {
    sessions.sort_by_key(|session| (!session["current"].as_bool().unwrap_or(false), !session["connected"].as_bool().unwrap_or(false), std::cmp::Reverse(instant(session, "issuedAt"))));
}

pub fn can_revoke_others(sessions: &[Value]) -> bool {
    sessions.iter().any(|session| !session["current"].as_bool().unwrap_or(false))
}

pub fn summarize(clients: usize, links: usize) -> String {
    let mut text = format!("{clients} {}", if clients == 1 { "client" } else { "clients" });
    if links > 0 {
        text.push_str(&format!(" \u{b7} {links} {}", if links == 1 { "pairing link" } else { "pairing links" }));
    }
    text
}

/// A client's name: the label it gave, else its system and browser, else who it signed in as.
pub fn client_label(session: &Value) -> String {
    let client = &session["client"];
    if let Some(label) = client["label"].as_str().filter(|label| !label.is_empty()) {
        return label.to_owned();
    }
    let parts: Vec<&str> = ["os", "browser"].iter().filter_map(|key| client[*key].as_str()).collect();
    if parts.is_empty() { session["subject"].as_str().unwrap_or("Client").to_owned() } else { parts.join(" \u{b7} ") }
}

/// Device type, system, browser and address, for the line under a client's name.
pub fn client_details(session: &Value) -> Vec<String> {
    let client = &session["client"];
    let mut bits = Vec::new();
    if let Some(device) = client["deviceType"].as_str().filter(|device| *device != "unknown") {
        let mut characters = device.chars();
        bits.push(characters.next().map(|first| first.to_uppercase().collect::<String>() + characters.as_str()).unwrap_or_default());
    }
    for key in ["os", "browser", "ipAddress"] {
        if let Some(value) = client[key].as_str() {
            bits.push(value.to_owned());
        }
    }
    bits
}

/// The scopes of a link or client, whichever field the server filled (`permissions` supersedes `scopes`).
pub fn scopes_of(record: &Value) -> Vec<String> {
    record.get("permissions").filter(|value| value.is_array()).or_else(|| record.get("scopes")).and_then(Value::as_array).map(|scopes| scopes.iter().filter_map(|scope| scope.as_str().map(str::to_owned)).collect()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn using_terminals_needs_viewing_them_and_viewing_ends_using() {
        let on = toggle(&[OrchestrationRead], TerminalOperate, true);
        assert!(on.contains(&TerminalOperate) && on.contains(&TerminalRead));
        let off = toggle(&on, TerminalRead, false);
        assert!(!off.contains(&TerminalOperate) && !off.contains(&TerminalRead) && off.contains(&OrchestrationRead));
        assert_eq!(toggle(&[SettingsWrite], SettingsWrite, true), [SettingsWrite]);
    }

    #[test]
    fn a_link_counts_down_to_its_expiry_with_the_originals_words() {
        for (seconds, label) in [(-1, "Expired"), (3, "Expires in a moment"), (42, "Expires in 42s"), (300, "Expires in 5m"), (252, "Expires in 4m 12s"), (3700, "Expires in 1h 1m 40s"), (7200, "Expires in 2h"), (90_000, "Expires in 1d 1h"), (86_400, "Expires in 1d")] {
            assert_eq!(expires_in(seconds * 1000, 0), label, "{seconds}");
        }
    }

    #[test]
    fn links_and_clients_are_ordered_and_named_as_in_the_original() {
        let mut links = vec![json!({"id":"a","createdAt":"2026-01-01T00:00:00Z"}), json!({"id":"b","createdAt":"2026-01-02T00:00:00Z"})];
        sort_links(&mut links);
        assert_eq!(links[0]["id"], "b");
        let mut clients = vec![
            json!({"sessionId":"old","current":false,"connected":false,"issuedAt":"2026-01-03T00:00:00Z"}),
            json!({"sessionId":"live","current":false,"connected":true,"issuedAt":"2026-01-01T00:00:00Z"}),
            json!({"sessionId":"me","current":true,"connected":true,"issuedAt":"2026-01-02T00:00:00Z"}),
        ];
        sort_clients(&mut clients);
        assert_eq!(clients.iter().map(|client| client["sessionId"].as_str().unwrap()).collect::<Vec<_>>(), ["me", "live", "old"]);
        assert!(can_revoke_others(&clients) && !can_revoke_others(&clients[..1]));
        assert_eq!(client_label(&json!({"client":{"os":"macOS","browser":"Safari"},"subject":"s"})), "macOS \u{b7} Safari");
        assert_eq!(client_label(&json!({"client":{"label":"Kitchen iPad"},"subject":"s"})), "Kitchen iPad");
        assert_eq!(client_label(&json!({"client":{},"subject":"owner"})), "owner");
        assert_eq!(client_details(&json!({"client":{"deviceType":"mobile","os":"iOS","ipAddress":"10.0.0.2"}})), ["Mobile", "iOS", "10.0.0.2"]);
        assert_eq!(summarize(1, 0), "1 client");
        assert_eq!(summarize(3, 2), "3 clients \u{b7} 2 pairing links");
        assert_eq!(pairing_url("http://127.0.0.1:3774/", "abc"), "http://127.0.0.1:3774/pair#token=abc");
        assert_eq!(scopes_of(&json!({"scopes":["a"],"permissions":["a","b"]})), ["a", "b"]);
        assert_eq!(wire(OrchestrationRead), "orchestration:read");
    }
}
