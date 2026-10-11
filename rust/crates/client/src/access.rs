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

/// The hosted app a link can open instead of this server's own page (`DEFAULT_HOSTED_APP_URL`). Only named in a link; never contacted.
pub const HOSTED_APP_URL: &str = "https://app.t3.codes";

/// How far an advertised endpoint reaches (`AdvertisedEndpointReachability`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reachability {
    Loopback,
    Lan,
    PrivateNetwork,
    Public,
}

/// An address clients can reach this machine at (`AdvertisedEndpoint`, `remoteAccess.ts`), as far as a link needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    pub id: String,
    pub label: String,
    pub http_base_url: String,
    pub provider_id: String,
    pub reachability: Reachability,
    /// `compatibility.hostedHttpsApp == "compatible"`: the hosted app can dial it.
    pub hosted_app_compatible: bool,
    /// False for `status == "unavailable"`.
    pub available: bool,
    pub is_default: bool,
}

impl Endpoint {
    pub fn parse(value: &Value) -> Option<Self> {
        let text = |name: &str| value[name].as_str().map(str::to_owned);
        Some(Self {
            id: text("id")?,
            label: text("label")?,
            http_base_url: text("httpBaseUrl")?,
            provider_id: value["provider"]["id"].as_str().unwrap_or_default().to_owned(),
            reachability: match value["reachability"].as_str()? {
                "loopback" => Reachability::Loopback,
                "lan" => Reachability::Lan,
                "private-network" => Reachability::PrivateNetwork,
                "public" => Reachability::Public,
                _ => return None,
            },
            hosted_app_compatible: value["compatibility"]["hostedHttpsApp"] == "compatible",
            available: value["status"] != "unavailable",
            is_default: value["isDefault"].as_bool().unwrap_or(false),
        })
    }

    /// `endpointDefaultPreferenceKey`: stable for a kind of endpoint, so a saved default survives its address changing.
    pub fn preference_key(&self) -> String {
        if self.id.starts_with("desktop-loopback:") {
            return "desktop-core:loopback:http".into();
        }
        if self.id.starts_with("desktop-lan:") {
            return "desktop-core:lan:http".into();
        }
        if self.id.starts_with("tailscale-ip:") {
            return "tailscale:ip:http".into();
        }
        if self.id.starts_with("tailscale-magicdns:") {
            return "tailscale:magicdns:https".into();
        }
        let scheme = url::Url::parse(&self.http_base_url).map_or("unknown".to_owned(), |url| url.scheme().to_owned());
        let reach = match self.reachability {
            Reachability::Loopback => "loopback",
            Reachability::Lan => "lan",
            Reachability::PrivateNetwork => "private-network",
            Reachability::Public => "public",
        };
        format!("{}:{reach}:{scheme}:{}", self.provider_id, self.label)
    }

    /// `isQrShareableEndpoint`: a code for a loopback address makes the scanning device dial itself.
    pub fn qr_shareable(&self) -> bool {
        self.available && self.reachability != Reachability::Loopback
    }

    /// `endpointShareHint`: who the link is for.
    pub fn share_hint(&self, url: &str) -> &'static str {
        if is_hosted_pairing_url(url) {
            return "Opens the hosted app, no install needed";
        }
        match self.reachability {
            Reachability::Lan => "Devices on the same network",
            Reachability::PrivateNetwork => "Devices on your private network",
            Reachability::Public => "Reachable from anywhere",
            Reachability::Loopback => "Clients on this machine",
        }
    }

    /// `resolveAdvertisedEndpointPairingUrl`.
    pub fn pairing_url(&self, credential: &str) -> String {
        if self.hosted_app_compatible {
            if let Some(url) = hosted_pairing_url(&self.http_base_url, credential) {
                return url;
            }
        }
        desktop_pairing_url(&self.http_base_url, credential).unwrap_or_else(|| pairing_url(&self.http_base_url, credential))
    }
}

/// `setPairingTokenOnUrl`: the credential in the fragment, which a server never sees in a request.
fn with_token(mut url: url::Url, credential: &str) -> String {
    let kept: Vec<(String, String)> = url.query_pairs().filter(|(name, _)| name != "token").map(|(name, value)| (name.into_owned(), value.into_owned())).collect();
    if kept.is_empty() {
        url.set_query(None);
    } else {
        url.query_pairs_mut().clear().extend_pairs(kept);
    }
    url.set_fragment(Some(&url::form_urlencoded::Serializer::new(String::new()).append_pair("token", credential).finish()));
    url.to_string()
}

/// `resolveDesktopPairingUrl`: the endpoint's own `/pair` page.
pub fn desktop_pairing_url(endpoint_url: &str, credential: &str) -> Option<String> {
    let mut url = url::Url::parse(endpoint_url).ok()?;
    url.set_path("/pair");
    Some(with_token(url, credential))
}

/// `resolveHostedPairingUrl`: the hosted app's `/pair` page told which host to pair with; only for an `https` endpoint.
pub fn hosted_pairing_url(endpoint_url: &str, credential: &str) -> Option<String> {
    if url::Url::parse(endpoint_url).ok()?.scheme() != "https" {
        return None;
    }
    let mut url = url::Url::parse(HOSTED_APP_URL).ok()?;
    url.set_path("/pair");
    url.query_pairs_mut().append_pair("host", endpoint_url);
    Some(with_token(url, credential))
}

/// `isHostedAppPairingUrl`.
pub fn is_hosted_pairing_url(value: &str) -> bool {
    url::Url::parse(value).is_ok_and(|url| url.path() == "/pair" && url.query_pairs().any(|(name, _)| name == "host"))
}

/// The browser's key for the interface state the original saves (`PERSISTED_STATE_KEY` of `uiStateStore.ts`).
pub const UI_STATE_KEY: &str = "t3code:ui-state:v1";

/// The saved default endpoint (`defaultAdvertisedEndpointKey`) in the saved interface state; none for a missing, unreadable or empty one.
pub fn stored_default_key(raw: Option<&str>) -> Option<String> {
    let value: Value = serde_json::from_str(raw?).ok()?;
    let key = value.get("defaultAdvertisedEndpointKey")?.as_str()?.trim();
    (!key.is_empty()).then(|| key.to_owned())
}

/// The saved interface state with the default endpoint set, every other field kept as it was (the original's store shares the one
/// record among its features). A record that cannot be read is replaced by one that holds only the key.
pub fn with_default_key(raw: Option<&str>, key: &str) -> String {
    let mut value = raw.and_then(|raw| serde_json::from_str::<Value>(raw).ok()).filter(Value::is_object).unwrap_or_else(|| serde_json::json!({}));
    value["defaultAdvertisedEndpointKey"] = serde_json::json!(key);
    value.to_string()
}

/// `selectPairingEndpoint`: the saved default among the endpoints that exist, else the one marked default, else the first
/// that is not loopback, else the first the hosted app can dial.
pub fn select_pairing_endpoint<'a>(endpoints: &'a [Endpoint], default_key: Option<&str>) -> Option<&'a Endpoint> {
    let available = || endpoints.iter().filter(|endpoint| endpoint.available);
    default_key
        .and_then(|key| available().find(|endpoint| endpoint.preference_key() == key))
        .or_else(|| available().find(|endpoint| endpoint.is_default))
        .or_else(|| available().find(|endpoint| endpoint.reachability != Reachability::Loopback))
        .or_else(|| available().find(|endpoint| endpoint.hosted_app_compatible))
}

/// One way of reaching this machine in the share panel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShareOption {
    pub id: String,
    pub preference_key: String,
    pub label: String,
    pub url: String,
    pub detail: &'static str,
    pub qr_shareable: bool,
}

/// `endpointCopyOptions`: a link for each endpoint that is not unavailable.
pub fn share_options(endpoints: &[Endpoint], credential: &str) -> Vec<ShareOption> {
    endpoints
        .iter()
        .filter(|endpoint| endpoint.available)
        .map(|endpoint| {
            let url = endpoint.pairing_url(credential);
            ShareOption { id: endpoint.id.clone(), preference_key: endpoint.preference_key(), label: endpoint.label.clone(), detail: endpoint.share_hint(&url), url, qr_shareable: endpoint.qr_shareable() }
        })
        .collect()
}

/// `selectQrEndpointOption`: the pick, else the saved default, else the first a code can be made for (so the panel does not
/// open on a loopback code), else the first. A pick that no longer exists falls back rather than blanking the panel.
pub fn select_share_option<'a>(options: &'a [ShareOption], selected: Option<&str>, default_key: Option<&str>) -> Option<&'a ShareOption> {
    selected
        .and_then(|id| options.iter().find(|option| option.id == id))
        .or_else(|| default_key.and_then(|key| options.iter().find(|option| option.preference_key == key)))
        .or_else(|| options.iter().find(|option| option.qr_shareable))
        .or_else(|| options.first())
}

/// Whether `origin` (`http://host:port`) is this computer, so a link made from it must not be a code.
pub fn is_loopback_origin(origin: &str) -> bool {
    url::Url::parse(origin).ok().and_then(|url| url.host_str().map(str::to_owned)).is_some_and(|host| matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]" | "::1") || host.ends_with(".localhost") || host.starts_with("127."))
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

    fn endpoint(value: Value) -> Endpoint {
        Endpoint::parse(&value).unwrap()
    }
    fn lan() -> Endpoint {
        endpoint(json!({"id":"desktop-lan:http://192.168.1.20:3774","label":"Local network","provider":{"id":"desktop-core"},"httpBaseUrl":"http://192.168.1.20:3774","reachability":"lan","compatibility":{"hostedHttpsApp":"mixed-content-blocked"},"status":"available","isDefault":true}))
    }
    fn loopback() -> Endpoint {
        endpoint(json!({"id":"desktop-loopback:3774","label":"This machine","provider":{"id":"desktop-core"},"httpBaseUrl":"http://127.0.0.1:3774","reachability":"loopback","compatibility":{"hostedHttpsApp":"mixed-content-blocked"},"status":"available"}))
    }
    fn tailnet() -> Endpoint {
        endpoint(json!({"id":"tailscale-magicdns:box.ts.net","label":"Tailscale HTTPS","provider":{"id":"tailscale"},"httpBaseUrl":"https://box.ts.net/","reachability":"private-network","compatibility":{"hostedHttpsApp":"compatible"},"status":"available"}))
    }

    #[test]
    fn a_link_for_an_endpoint_is_its_own_pair_page_or_the_hosted_apps_for_https() {
        assert_eq!(lan().pairing_url("abc"), "http://192.168.1.20:3774/pair#token=abc");
        assert_eq!(tailnet().pairing_url("abc"), "https://app.t3.codes/pair?host=https%3A%2F%2Fbox.ts.net%2F#token=abc");
        assert_eq!(desktop_pairing_url("https://box.ts.net/some/path?token=old&x=1", "a b"), Some("https://box.ts.net/pair?x=1#token=a+b".into()));
        assert_eq!(hosted_pairing_url("http://192.168.1.20:3774", "abc"), None, "the hosted app is https and cannot dial http");
        assert!(is_hosted_pairing_url(&tailnet().pairing_url("abc")) && !is_hosted_pairing_url(&lan().pairing_url("abc")));
        assert_eq!(tailnet().share_hint(&tailnet().pairing_url("abc")), "Opens the hosted app, no install needed");
        assert_eq!((lan().share_hint("x"), loopback().share_hint("x")), ("Devices on the same network", "Clients on this machine"));
    }

    #[test]
    fn the_endpoint_a_link_uses_follows_the_saved_default_then_the_marked_one_then_reach() {
        let all = [loopback(), lan(), tailnet()];
        assert_eq!(select_pairing_endpoint(&all, Some("tailscale:magicdns:https")).map(|e| e.label.as_str()), Some("Tailscale HTTPS"));
        assert_eq!(select_pairing_endpoint(&all, Some("gone")).map(|e| e.label.as_str()), Some("Local network"), "the marked default");
        assert_eq!(select_pairing_endpoint(&[loopback(), tailnet()], None).map(|e| e.label.as_str()), Some("Tailscale HTTPS"), "the first that is not loopback");
        assert_eq!(select_pairing_endpoint(&[loopback()], None), None);
        let mut gone = lan();
        gone.available = false;
        assert_eq!(select_pairing_endpoint(&[gone], None), None, "an unavailable endpoint is never chosen");
        assert_eq!(loopback().preference_key(), "desktop-core:loopback:http");
        let custom = endpoint(json!({"id":"user:1","label":"Office","provider":{"id":"manual"},"httpBaseUrl":"https://office.example","reachability":"public","compatibility":{"hostedHttpsApp":"unknown"},"status":"unknown"}));
        assert_eq!(custom.preference_key(), "manual:public:https:Office");
    }

    #[test]
    fn the_share_panel_opens_on_an_endpoint_a_code_can_be_made_for() {
        let options = share_options(&[loopback(), lan(), tailnet()], "abc");
        assert_eq!(options.len(), 3);
        assert!(!options[0].qr_shareable && options[1].qr_shareable);
        assert_eq!(select_share_option(&options, None, None).map(|o| o.label.as_str()), Some("Local network"), "not the loopback one that comes first");
        assert_eq!(select_share_option(&options, Some("desktop-loopback:3774"), None).map(|o| o.label.as_str()), Some("This machine"), "an explicit pick wins");
        assert_eq!(select_share_option(&options, Some("vanished"), Some("tailscale:magicdns:https")).map(|o| o.label.as_str()), Some("Tailscale HTTPS"), "a stale pick falls back to the saved default");
        assert_eq!(select_share_option(&share_options(&[loopback()], "abc"), None, None).map(|o| o.label.as_str()), Some("This machine"));
        assert_eq!(select_share_option(&[], None, None), None);
        assert!(is_loopback_origin("http://127.0.0.1:3774") && is_loopback_origin("http://localhost:3774") && is_loopback_origin("http://[::1]:3774"));
        assert!(!is_loopback_origin("http://192.168.1.20:3774") && !is_loopback_origin("https://box.ts.net"));
    }

    #[test]
    fn the_saved_default_endpoint_is_one_field_of_the_shared_interface_state() {
        assert_eq!(stored_default_key(None), None);
        assert_eq!(stored_default_key(Some("not json")), None);
        assert_eq!(stored_default_key(Some(r#"{"defaultAdvertisedEndpointKey":"  "}"#)), None);
        assert_eq!(stored_default_key(Some(r#"{"defaultAdvertisedEndpointKey":"manual:public:https:Office","projectOrder":["a"]}"#)).as_deref(), Some("manual:public:https:Office"));
        // Setting it keeps what else the record holds.
        let saved = with_default_key(Some(r#"{"projectOrder":["a"],"sidebarProjectScopeKey":"k"}"#), "tailscale:serve:https:Tailnet");
        let value: Value = serde_json::from_str(&saved).unwrap();
        assert_eq!(value["projectOrder"], serde_json::json!(["a"]));
        assert_eq!(value["sidebarProjectScopeKey"], "k");
        assert_eq!(stored_default_key(Some(&saved)).as_deref(), Some("tailscale:serve:https:Tailnet"));
        // A record that cannot be read is replaced, not extended.
        assert_eq!(with_default_key(Some("[1]"), "x"), r#"{"defaultAdvertisedEndpointKey":"x"}"#);
    }
}
