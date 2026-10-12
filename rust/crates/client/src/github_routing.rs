//! Sharing GitHub access between environments: what a machine's pull request reads and writes may use of another machine's signed-in
//! account, and the rules that decide which machine answers one. Port of `packages/client-runtime/src/connection/githubRoutingPermissions.ts`
//! and the pure parts of `state/pullRequestRouting.ts`; the requests themselves are `t3-ui`'s `pull_request_routing`.
//!
//! Trust belongs to a machine's saved endpoints, never to an environment id a server advertises. Credentials stay on their machines:
//! only a verified host and account id cross between them.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// How much of a machine's GitHub access other machines may use through this client (`GitHubRoutingPermission`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Permission {
    #[default]
    #[serde(rename = "off")]
    Off,
    #[serde(rename = "read")]
    Read,
    #[serde(rename = "read-write")]
    ReadWrite,
}

impl Permission {
    pub const ALL: [Permission; 3] = [Self::Off, Self::Read, Self::ReadWrite];
    pub fn wire(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Read => "read",
            Self::ReadWrite => "read-write",
        }
    }
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|permission| permission.wire() == text)
    }
    /// The words of the choice (`options` of `GitHubRoutingSettings.tsx`).
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Read => "Read PRs",
            Self::ReadWrite => "Read and act",
        }
    }
}

/// `StoredGitHubRoutingPermission`: the trust given to one environment, tied to the endpoints it was given for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Stored {
    pub environment_id: String,
    pub connection_key: String,
    pub permission: Permission,
}

/// One way an environment is reached, as far as trust is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// The environment the app itself came from or started (never saved).
    Primary { environment: String, http_base_url: String, ws_base_url: String },
    Bearer { environment: String, http_base_url: String, ws_base_url: String },
    Relay { environment: String },
    Ssh { environment: String, alias: String, hostname: String, username: Option<String>, port: Option<i64> },
}

/// `new URL(base).href` without trailing slashes; none for a base that is not an http(s)/ws(s) address with no credentials in it.
fn clean(base: &str, schemes: [&str; 2]) -> Option<String> {
    let url = url::Url::parse(base).ok()?;
    (schemes.contains(&url.scheme()) && url.username().is_empty() && url.password().is_none()).then(|| url.as_str().trim_end_matches('/').to_owned())
}

fn route_key(endpoint: &Endpoint) -> Option<String> {
    let encoded = |value: Value| serde_json::to_string(&value).ok();
    match endpoint {
        Endpoint::Relay { environment } => encoded(json!(["RelayConnectionTarget", environment])),
        Endpoint::Ssh { environment, alias, hostname, username, port } => encoded(json!(["SshConnectionTarget", environment, alias, hostname, username, port])),
        Endpoint::Primary { environment, http_base_url, ws_base_url } => encoded(json!(["PrimaryConnectionTarget", environment, clean(http_base_url, ["http", "https"])?, clean(ws_base_url, ["ws", "wss"])?])),
        Endpoint::Bearer { environment, http_base_url, ws_base_url } => encoded(json!(["BearerConnectionTarget", environment, clean(http_base_url, ["http", "https"])?, clean(ws_base_url, ["ws", "wss"])?])),
    }
}

/// `gitHubRoutingConnectionKey`: with several routes the key covers all of them, sorted, so reordering keeps trust but adding or
/// changing an address revokes it. The caller leaves out learned routes, which come and go with the server's addresses.
pub fn connection_key(endpoints: &[Endpoint]) -> Option<String> {
    match endpoints {
        [] => None,
        [only] => route_key(only),
        many => {
            let mut keys = many.iter().map(route_key).collect::<Option<Vec<_>>>()?;
            keys.sort();
            serde_json::to_string(&keys).ok()
        }
    }
}

/// The websocket base that goes with an http one.
pub fn ws_base_of(http_base_url: &str) -> String {
    match http_base_url.split_once("://") {
        Some(("https", rest)) => format!("wss://{rest}"),
        Some((_, rest)) => format!("ws://{rest}"),
        None => http_base_url.to_owned(),
    }
}

/// `gitHubRoutingPermissionFor`: what is granted to the environment whose endpoints make `key`; nothing without a key or a matching grant.
pub fn permission_for(key: Option<&str>, stored: &[Stored]) -> Permission {
    key.and_then(|key| stored.iter().find(|grant| grant.connection_key == key)).map_or(Permission::Off, |grant| grant.permission)
}

/// The grants after `environment` is given `permission` (one grant per environment; Off removes it).
pub fn with_permission(stored: &[Stored], environment: &str, key: &str, permission: Permission) -> Vec<Stored> {
    let mut next: Vec<Stored> = stored.iter().filter(|grant| grant.environment_id != environment).cloned().collect();
    if permission != Permission::Off {
        next.push(Stored { environment_id: environment.to_owned(), connection_key: key.to_owned(), permission });
    }
    next
}

/// `routingAllowed`'s permission rule: a read needs both machines on at least Read, an act needs both on Read and act.
pub fn allowed(source: Permission, target: Permission, write: bool) -> bool {
    if write { source == Permission::ReadWrite && target == Permission::ReadWrite } else { source != Permission::Off && target != Permission::Off }
}

/// The reads that may be answered by another machine (`reads` of `pullRequestRouting.ts`).
pub const READS: [&str; 11] = [
    "pullRequests.summary",
    "pullRequests.stack",
    "pullRequests.detail",
    "pullRequests.preview",
    "pullRequests.checks",
    "pullRequests.activity",
    "pullRequests.threadComments",
    "pullRequests.diffFileContents",
    "pullRequests.filesViewed",
    "pullRequests.reviewerCandidates",
    "pullRequests.labelCandidates",
];
/// The writes that may be done through another machine (`writes`).
pub const WRITES: [&str; 11] = [
    "pullRequests.setFilesViewed",
    "pullRequests.runAction",
    "pullRequests.update",
    "pullRequests.comment",
    "pullRequests.updateComment",
    "pullRequests.submitReview",
    "pullRequests.replyToThread",
    "pullRequests.setReaction",
    "pullRequests.setThreadResolution",
    "pullRequests.requestReviewers",
    "pullRequests.setLabels",
];
pub fn is_read(method: &str) -> bool {
    READS.contains(&method)
}
pub fn is_write(method: &str) -> bool {
    WRITES.contains(&method)
}

/// `isRef`: the input names one pull request.
pub fn is_reference(payload: &Value) -> bool {
    payload["projectId"].is_string() && payload["repository"].as_str().is_some_and(|repository| !repository.is_empty()) && payload["number"].is_u64()
}

/// `isLocal`: an environment on this machine (its address is a loopback host), which is tried first for a read because it costs nothing.
pub fn is_local(http_base_url: &str) -> bool {
    url::Url::parse(http_base_url).ok().and_then(|url| url.host_str().map(str::to_lowercase)).is_some_and(|host| matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]" | "::1"))
}

/// `matchesReference`: two references name the same pull request (the host is compared only when the filter names one).
pub fn matches_reference(reference: &Value, filter: &Value) -> bool {
    let lower = |value: &Value| value.as_str().map(str::to_lowercase);
    reference["projectId"] == filter["projectId"]
        && lower(&reference["repository"]) == lower(&filter["repository"])
        && reference["number"] == filter["number"]
        && (filter["host"].is_null() || lower(&reference["host"]) == lower(&filter["host"]))
}

/// A machine that could answer, other than the one asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alternative {
    pub environment: String,
    pub local: bool,
}

/// The machines to try, in order (`candidates` of `routedRequest`): an act goes first to the origin when the origin is on this machine,
/// then to the machines on this machine, then to the origin, then to the others; a read tries the local machines, then the others,
/// and falls back to the origin last.
pub fn candidates(origin: &str, origin_local: bool, alternatives: &[Alternative], write: bool) -> Vec<String> {
    let local = alternatives.iter().filter(|alternative| alternative.local).map(|alternative| alternative.environment.clone());
    let remote = alternatives.iter().filter(|alternative| !alternative.local).map(|alternative| alternative.environment.clone());
    let origin = origin.to_owned();
    let mut order = Vec::new();
    if origin_local && write {
        order.push(origin.clone());
    }
    order.extend(local);
    if !origin_local && write {
        order.push(origin.clone());
    }
    order.extend(remote);
    if !write {
        order.push(origin);
    }
    order
}

/// `summarizeGitHubRouting`: the machines that share, grouped by what they share; none when nothing is shared.
pub fn summarize(entries: &[(String, Permission)]) -> Option<String> {
    let groups: Vec<String> = [(Permission::ReadWrite, "read and act"), (Permission::Read, "read PRs")]
        .into_iter()
        .filter_map(|(permission, words)| {
            let labels: Vec<&str> = entries.iter().filter(|(_, granted)| *granted == permission).map(|(label, _)| label.as_str()).collect();
            (!labels.is_empty()).then(|| format!("{} {words}", labels.join(", ")))
        })
        .collect();
    (!groups.is_empty()).then(|| groups.join(" \u{b7} "))
}

/// Whether another machine's answer to `pullRequests.routingIdentity` is the account the origin named for the project: the same provider,
/// the same host (any case) and the same account id. Anything else is a machine that must not be used.
pub fn same_account(origin: &Value, other: &Value) -> bool {
    let host = |value: &Value| value["host"].as_str().map(str::to_lowercase);
    other["provider"] == "github" && host(other).is_some() && host(other) == host(origin) && other["accountId"].is_string() && other["accountId"] == origin["accountId"]
}

/// Whether an error message is the server refusing before it started (the account it was asked to use is not the signed-in one),
/// which is the only failure of an act that may be tried on the next machine (`rejectedBeforeDispatch`).
pub fn rejected_before_dispatch(message: &str) -> bool {
    message.contains("The GitHub account could not be verified before starting the operation.")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bearer(environment: &str, http: &str) -> Endpoint {
        Endpoint::Bearer { environment: environment.into(), http_base_url: http.into(), ws_base_url: ws_base_of(http) }
    }

    #[test]
    fn trust_follows_the_endpoints_not_the_environment_id() {
        let a = bearer("env", "https://a.example/");
        let moved = bearer("env", "https://b.example/");
        let key = connection_key(std::slice::from_ref(&a)).unwrap();
        let stored = with_permission(&[], "env", &key, Permission::Read);
        assert_eq!(permission_for(Some(&key), &stored), Permission::Read);
        // Another address of the same environment id has no grant until the person gives one.
        assert_eq!(permission_for(connection_key(&[moved]).as_deref(), &stored), Permission::Off);
        // More routes make another key, in whatever order they are listed.
        let both = connection_key(&[a.clone(), bearer("env", "https://c.example/")]).unwrap();
        assert_eq!(Some(both.clone()), connection_key(&[bearer("env", "https://c.example/"), a.clone()]));
        assert_eq!(permission_for(Some(&both), &stored), Permission::Off);
        // An address with credentials in it, or one that is not a web address, has no key and so no trust.
        assert_eq!(connection_key(&[bearer("env", "https://user:pw@a.example/")]), None);
        assert_eq!(connection_key(&[]), None);
        assert_eq!(permission_for(None, &stored), Permission::Off);
    }

    #[test]
    fn a_grant_is_replaced_per_environment_and_off_removes_it() {
        let first = with_permission(&[], "a", "ka", Permission::Read);
        let second = with_permission(&first, "b", "kb", Permission::ReadWrite);
        let changed = with_permission(&second, "a", "ka", Permission::ReadWrite);
        assert_eq!(changed.iter().map(|grant| (grant.environment_id.as_str(), grant.permission)).collect::<Vec<_>>(), [("b", Permission::ReadWrite), ("a", Permission::ReadWrite)]);
        assert_eq!(with_permission(&changed, "a", "ka", Permission::Off).len(), 1);
    }

    #[test]
    fn a_read_needs_both_machines_on_and_an_act_needs_both_to_read_and_act() {
        use Permission::*;
        assert!(allowed(Read, Read, false) && allowed(ReadWrite, Read, false));
        assert!(!allowed(Off, Read, false) && !allowed(Read, Off, false));
        assert!(allowed(ReadWrite, ReadWrite, true));
        assert!(!allowed(Read, ReadWrite, true) && !allowed(ReadWrite, Read, true));
    }

    #[test]
    fn machines_are_tried_in_the_originals_order() {
        let others = [Alternative { environment: "near".into(), local: true }, Alternative { environment: "far".into(), local: false }];
        assert_eq!(candidates("here", true, &others, true), ["here", "near", "far"]);
        assert_eq!(candidates("here", false, &others, true), ["near", "here", "far"]);
        assert_eq!(candidates("here", false, &others, false), ["near", "far", "here"]);
        assert_eq!(candidates("here", true, &others, false), ["near", "far", "here"]);
    }

    #[test]
    fn references_match_by_project_repository_number_and_a_named_host() {
        let reference = json!({"projectId": "p", "repository": "Acme/Web", "number": 3, "host": "GitHub.com"});
        assert!(matches_reference(&reference, &json!({"projectId": "p", "repository": "acme/web", "number": 3})));
        assert!(matches_reference(&reference, &json!({"projectId": "p", "repository": "acme/web", "number": 3, "host": "github.com"})));
        assert!(!matches_reference(&reference, &json!({"projectId": "p", "repository": "acme/web", "number": 4})));
        assert!(!matches_reference(&reference, &json!({"projectId": "q", "repository": "acme/web", "number": 3})));
        assert!(!matches_reference(&reference, &json!({"projectId": "p", "repository": "acme/web", "number": 3, "host": "ghe.example"})));
    }

    #[test]
    fn only_the_same_account_on_the_same_host_is_the_same_account() {
        let origin = json!({"provider": "github", "host": "github.com", "accountId": "42", "viewer": "octo"});
        assert!(same_account(&origin, &json!({"provider": "github", "host": "GitHub.com", "accountId": "42", "viewer": "octo-too"})));
        assert!(!same_account(&origin, &json!({"provider": "github", "host": "github.com", "accountId": "43"})), "another account");
        assert!(!same_account(&origin, &json!({"provider": "github", "host": "ghe.example", "accountId": "42"})), "another host");
        assert!(!same_account(&origin, &json!({"provider": "gitlab", "host": "github.com", "accountId": "42"})), "another provider");
        assert!(!same_account(&origin, &json!({"provider": "github", "host": "github.com"})), "no account");
        assert!(!same_account(&json!({}), &json!({"provider": "github"})), "no host either side");
    }

    #[test]
    fn only_loopback_machines_are_local_and_the_summary_names_who_shares() {
        assert!(is_local("http://localhost:3773/") && is_local("http://127.0.0.1:1/") && is_local("http://[::1]:1/"));
        assert!(!is_local("http://192.168.1.4:3773/") && !is_local("not a url"));
        let entries = [("Laptop".to_owned(), Permission::ReadWrite), ("Desk".to_owned(), Permission::Read), ("Box".to_owned(), Permission::Off), ("Pi".to_owned(), Permission::Read)];
        assert_eq!(summarize(&entries).as_deref(), Some("Laptop read and act \u{b7} Desk, Pi read PRs"));
        assert_eq!(summarize(&entries[2..3]), None);
    }
}
