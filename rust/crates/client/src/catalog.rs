//! The saved environments, their routes and their credentials, as the one document that is persisted (port of
//! `packages/client-runtime/src/platform/storageDocument.ts`, `connection/catalog.ts`, `connection/model.ts` and the pure parts of
//! `connection/routes.ts`). The JSON is the original's `ConnectionCatalogDocument`, field for field, so a document the original
//! wrote reads here and the reverse; the platform decides where the text lives and how it is protected (the web build keeps it in
//! browser storage as the original does, the desktop app in an encrypted file: `t3-ui`'s `connection_store`).
//!
//! An environment is saved as one or more routes (T3 Connect, a direct address, SSH), preferred first. A route owns a profile (its
//! address) and, for a direct route, a credential. Nothing here reads a clock, a disk or the network.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use t3_contracts::EnvironmentId;

/// `PersistedConnectionTarget`: one way to reach an environment. The primary (same-origin or embedded) environment is never saved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "_tag", rename_all_fields = "camelCase")]
pub enum Target {
    #[serde(rename = "BearerConnectionTarget")]
    Bearer { environment_id: EnvironmentId, label: String, connection_id: String },
    #[serde(rename = "RelayConnectionTarget")]
    Relay { environment_id: EnvironmentId, label: String },
    #[serde(rename = "SshConnectionTarget")]
    Ssh { environment_id: EnvironmentId, label: String, connection_id: String },
}

impl Target {
    pub fn environment_id(&self) -> &EnvironmentId {
        match self {
            Self::Bearer { environment_id, .. } | Self::Relay { environment_id, .. } | Self::Ssh { environment_id, .. } => environment_id,
        }
    }
    pub fn label(&self) -> &str {
        match self {
            Self::Bearer { label, .. } | Self::Relay { label, .. } | Self::Ssh { label, .. } => label,
        }
    }
    /// The id that names the route's profile and credential; a T3 Connect route has neither.
    pub fn connection_id(&self) -> Option<&str> {
        match self {
            Self::Bearer { connection_id, .. } | Self::Ssh { connection_id, .. } => Some(connection_id),
            Self::Relay { .. } => None,
        }
    }
    /// `routeKey`: what tells routes of one environment apart.
    fn route_key(&self) -> String {
        self.connection_id().map_or_else(|| RELAY_ROUTE_ID.to_owned(), str::to_owned)
    }
}

/// `DesktopSshEnvironmentTarget`: `username` and `port` are present and null when there is none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshTarget {
    pub alias: String,
    pub hostname: String,
    pub username: Option<String>,
    pub port: Option<i64>,
}

/// `ConnectionProfile`: where a route is, apart from who it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "_tag", rename_all_fields = "camelCase")]
pub enum Profile {
    #[serde(rename = "BearerConnectionProfile")]
    Bearer {
        connection_id: String,
        environment_id: EnvironmentId,
        label: String,
        http_base_url: String,
        ws_base_url: String,
        /// Set on a route the server reported while this client was connected, rather than one the person paired.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        learned: Option<bool>,
        /// `"t3-connect"` when the route authenticates with the environment's T3 Connect credential, not a stored token.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        authorization: Option<String>,
    },
    #[serde(rename = "SshConnectionProfile")]
    Ssh { connection_id: String, environment_id: EnvironmentId, label: String, target: SshTarget },
}

impl Profile {
    pub fn connection_id(&self) -> &str {
        match self {
            Self::Bearer { connection_id, .. } | Self::Ssh { connection_id, .. } => connection_id,
        }
    }
}

/// `ConnectionCredential`: the token a direct route authenticates with. It is a secret.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "_tag")]
pub enum Credential {
    #[serde(rename = "BearerConnectionCredential")]
    Bearer { token: String },
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Credential(..)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredCredential {
    pub connection_id: String,
    pub credential: Credential,
}

/// `ConnectionCatalogDocument`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Document {
    pub schema_version: u8,
    pub targets: Vec<Target>,
    pub profiles: Vec<Profile>,
    pub credentials: Vec<StoredCredential>,
    /// T3 Connect tokens, carried as the original wrote them: this app cannot sign in to T3 Connect, and does not touch them.
    pub remote_dpop_tokens: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_routing_permissions: Option<Vec<Value>>,
    /// Saved environments the person switched off: kept with their credentials, never connected.
    #[serde(default)]
    pub disabled_environment_ids: Vec<EnvironmentId>,
}

impl Default for Document {
    fn default() -> Self {
        Self { schema_version: 1, targets: vec![], profiles: vec![], credentials: vec![], remote_dpop_tokens: vec![], github_routing_permissions: None, disabled_environment_ids: vec![] }
    }
}

/// What a person registers: one route of an environment, with what it needs.
#[derive(Debug, Clone, PartialEq)]
pub enum Registration {
    Relay { target: Target },
    Bearer { target: Target, profile: Profile, credential: Credential },
    Ssh { target: Target, profile: Profile },
}

impl Document {
    pub fn parse(raw: &str) -> Result<Self, String> {
        let document: Self = serde_json::from_str(raw).map_err(|error| format!("The saved connections could not be read: {error}"))?;
        if document.schema_version != 1 {
            return Err(format!("The saved connections use schema version {}, which this app does not know.", document.schema_version));
        }
        Ok(document)
    }
    pub fn encode(&self) -> String {
        serde_json::to_string(self).expect("a catalog document serializes")
    }

    /// `catalogRoutes`: an environment's saved targets in preference order.
    pub fn routes_of(&self, environment: &EnvironmentId) -> Vec<&Target> {
        self.targets.iter().filter(|target| target.environment_id() == environment).collect()
    }

    /// `setRoutesInCatalog`: replaces an environment's routes with `routes`, preferred first. What a dropped route owned goes with it;
    /// the environment keeps its place in the catalog.
    pub fn set_routes(&mut self, environment: &EnvironmentId, routes: Vec<Target>) {
        let kept: Vec<String> = routes.iter().map(Target::route_key).collect();
        let dropped: Vec<Target> = self.routes_of(environment).into_iter().filter(|target| !kept.contains(&target.route_key())).cloned().collect();
        let first = self.targets.iter().position(|target| target.environment_id() == environment);
        let others: Vec<Target> = self.targets.iter().filter(|target| target.environment_id() != environment).cloned().collect();
        let insert_at = match first {
            None => others.len(),
            Some(index) => self.targets[..index].iter().filter(|target| target.environment_id() != environment).count(),
        };
        self.remove_route_metadata(&dropped);
        let mut targets = others;
        targets.splice(insert_at..insert_at, routes);
        self.targets = targets;
    }

    fn remove_route_metadata(&mut self, removed: &[Target]) {
        let ids: Vec<&str> = removed.iter().filter_map(Target::connection_id).collect();
        let relay_removed = removed.iter().any(|target| matches!(target, Target::Relay { .. }));
        self.profiles.retain(|profile| !ids.contains(&profile.connection_id()));
        self.credentials.retain(|stored| !ids.contains(&stored.connection_id.as_str()));
        // The T3 Connect token belongs to the T3 Connect route.
        if relay_removed {
            let environments: Vec<&EnvironmentId> = removed.iter().map(Target::environment_id).collect();
            self.remote_dpop_tokens.retain(|token| !environments.iter().any(|environment| token["environmentId"] == environment.as_str()));
        }
    }

    /// `registerConnectionInCatalog`: saves one route of an environment, keeping its other routes (`routes` is the whole list, the
    /// registered one among them). Re-registering keeps the switched-off flag.
    pub fn register(&mut self, registration: Registration, routes: Option<Vec<Target>>) {
        let target = match &registration {
            Registration::Relay { target } | Registration::Bearer { target, .. } | Registration::Ssh { target, .. } => target.clone(),
        };
        let environment = target.environment_id().clone();
        self.set_routes(&environment, routes.unwrap_or_else(|| vec![target]));
        match registration {
            Registration::Relay { .. } => {}
            Registration::Bearer { target, profile, credential } => {
                replace(&mut self.profiles, |profile| profile.connection_id().to_owned(), profile);
                let connection_id = target.connection_id().expect("a bearer route has an id").to_owned();
                replace(&mut self.credentials, |stored| stored.connection_id.clone(), StoredCredential { connection_id, credential });
            }
            Registration::Ssh { profile, .. } => replace(&mut self.profiles, |profile| profile.connection_id().to_owned(), profile),
        }
    }

    /// `removeConnectionFromCatalog`: forgets an environment and every route it had.
    pub fn remove(&mut self, environment: &EnvironmentId) {
        self.set_routes(environment, vec![]);
        self.remote_dpop_tokens.retain(|token| token["environmentId"] != environment.as_str());
        self.disabled_environment_ids.retain(|id| id != environment);
        if let Some(permissions) = &mut self.github_routing_permissions {
            permissions.retain(|permission| permission["environmentId"] != environment.as_str());
        }
    }

    /// `setConnectionEnabledInCatalog`: unknown environments are ignored.
    pub fn set_enabled(&mut self, environment: &EnvironmentId, enabled: bool) {
        let registered = self.targets.iter().any(|target| target.environment_id() == environment);
        self.disabled_environment_ids.retain(|id| id != environment);
        if registered && !enabled {
            self.disabled_environment_ids.push(environment.clone());
        }
    }

    pub fn enabled(&self, environment: &EnvironmentId) -> bool {
        !self.disabled_environment_ids.contains(environment)
    }

    pub fn profile(&self, connection_id: &str) -> Option<&Profile> {
        self.profiles.iter().find(|profile| profile.connection_id() == connection_id)
    }

    /// The token a direct route authenticates with: its own, or the one of the route it was learned over (`credentialConnectionId`).
    pub fn token(&self, connection_id: &str) -> Option<&str> {
        let owner = credential_connection_id(connection_id);
        self.credentials.iter().find(|stored| stored.connection_id == owner).map(|stored| match &stored.credential {
            Credential::Bearer { token } => token.as_str(),
        })
    }

    /// The saved environments in catalog order, each with its routes preferred first.
    pub fn entries(&self) -> Vec<Entry> {
        let mut order: Vec<&EnvironmentId> = vec![];
        for target in &self.targets {
            if !order.contains(&target.environment_id()) {
                order.push(target.environment_id());
            }
        }
        order
            .into_iter()
            .map(|environment| Entry {
                environment: environment.clone(),
                label: self.routes_of(environment).first().map(|target| target.label().to_owned()).unwrap_or_default(),
                routes: self.routes_of(environment).into_iter().map(|target| Route { target: target.clone(), profile: target.connection_id().and_then(|id| self.profile(id)).cloned() }).collect(),
                enabled: self.enabled(environment),
            })
            .collect()
    }
}

fn replace<A>(values: &mut Vec<A>, key: impl Fn(&A) -> String, next: A) {
    let next_key = key(&next);
    values.retain(|value| key(value) != next_key);
    values.push(next);
}

/// An environment has at most one T3 Connect route, so it needs no per-route id.
pub const RELAY_ROUTE_ID: &str = "relay";

#[derive(Debug, Clone, PartialEq)]
pub struct Route {
    pub target: Target,
    pub profile: Option<Profile>,
}

/// A saved environment: its routes, preferred first (`ConnectionCatalogEntry`, whose `target`/`profile` is `routes[0]`).
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub environment: EnvironmentId,
    pub label: String,
    pub routes: Vec<Route>,
    pub enabled: bool,
}

/// `credentialConnectionId`: a learned route borrows the credential of the route it was learned over, whose id follows the first `@`.
pub fn credential_connection_id(connection_id: &str) -> &str {
    match connection_id.find('@') {
        Some(at) if connection_id.starts_with("learned:") => &connection_id[at + 1..],
        _ => connection_id,
    }
}

/// `bearerConnectionId`: one id per environment and origin, so pairing the same address again replaces that route.
pub fn bearer_connection_id(environment: &EnvironmentId, http_base_url: &str) -> String {
    let origin = url::Url::parse(http_base_url).map(|url| url.origin().ascii_serialization()).unwrap_or_else(|_| http_base_url.trim_end_matches('/').to_owned());
    format!("bearer:{environment}:{origin}")
}

/// A direct route for a paired address: the registration the original makes after a bearer token was issued.
pub fn bearer_registration(environment: &EnvironmentId, label: &str, http_base_url: &str, token: &str) -> Registration {
    let connection_id = bearer_connection_id(environment, http_base_url);
    let ws_base_url = url::Url::parse(http_base_url)
        .ok()
        .map(|mut url| {
            let _ = url.set_scheme(if url.scheme() == "https" { "wss" } else { "ws" });
            url.to_string()
        })
        .unwrap_or_default();
    Registration::Bearer {
        target: Target::Bearer { environment_id: environment.clone(), label: label.to_owned(), connection_id: connection_id.clone() },
        profile: Profile::Bearer { connection_id, environment_id: environment.clone(), label: label.to_owned(), http_base_url: http_base_url.to_owned(), ws_base_url, learned: None, authorization: None },
        credential: Credential::Bearer { token: token.to_owned() },
    }
}

// ---- routes ----------------------------------------------------------------------------------------------------------------

/// `ConnectionRouteKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteKind {
    Relay,
    Loopback,
    Lan,
    Tailnet,
    Public,
    Ssh,
}

impl RouteKind {
    /// `ROUTE_KIND_RANK`: faster first.
    fn rank(self) -> u8 {
        match self {
            Self::Loopback => 0,
            Self::Lan => 1,
            Self::Tailnet => 2,
            Self::Public => 3,
            Self::Ssh => 4,
            Self::Relay => 5,
        }
    }
}

impl Route {
    pub fn id(&self) -> String {
        self.target.route_key()
    }
    /// `routeHttpBaseUrl`: the base URL of a direct route, none for T3 Connect and SSH.
    pub fn http_base_url(&self) -> Option<&str> {
        match &self.profile {
            Some(Profile::Bearer { http_base_url, .. }) => Some(http_base_url),
            _ => None,
        }
    }
    fn hostname(&self) -> Option<String> {
        url::Url::parse(self.http_base_url()?).ok()?.host_str().map(str::to_owned)
    }
    /// `connectionRouteKind`.
    pub fn kind(&self) -> RouteKind {
        match &self.target {
            Target::Relay { .. } => RouteKind::Relay,
            Target::Ssh { .. } => RouteKind::Ssh,
            Target::Bearer { .. } => match self.hostname() {
                None => RouteKind::Public,
                Some(host) if crate::host_class::is_local_loopback(&host) => RouteKind::Loopback,
                Some(host) if crate::host_class::is_tailnet(&host) => RouteKind::Tailnet,
                Some(host) if crate::host_class::is_private_network(&host) => RouteKind::Lan,
                Some(_) => RouteKind::Public,
            },
        }
    }
    /// `connectionRouteLabel`: "LAN", "Tailscale", "T3 Connect", a host, or an SSH host.
    pub fn label(&self) -> String {
        match self.kind() {
            RouteKind::Relay => "T3 Connect".into(),
            RouteKind::Loopback => "This device".into(),
            RouteKind::Lan => "LAN".into(),
            RouteKind::Tailnet => "Tailscale".into(),
            RouteKind::Ssh => match &self.profile {
                Some(Profile::Ssh { target, .. }) => format!("SSH {}{}", target.username.as_deref().filter(|name| !name.is_empty()).map(|name| format!("{name}@")).unwrap_or_default(), target.hostname),
                _ => "SSH".into(),
            },
            RouteKind::Public => self.hostname().unwrap_or_else(|| "Remote link".into()),
        }
    }
    /// `connectionRouteAddress`: the address shown under a route, none for T3 Connect.
    pub fn address(&self) -> Option<String> {
        match (&self.target, &self.profile) {
            (Target::Ssh { .. }, Some(Profile::Ssh { target, .. })) => Some(target.alias.clone()),
            (Target::Ssh { .. }, _) => None,
            _ => self.http_base_url().map(str::to_owned),
        }
    }
    /// A learned route (`isLearned`).
    pub fn learned(&self) -> bool {
        matches!(&self.profile, Some(Profile::Bearer { learned: Some(true), .. }))
    }
    /// `routeAddressKey`: two routes with the same key reach the same address.
    fn address_key(&self) -> Option<String> {
        match &self.profile {
            Some(Profile::Bearer { http_base_url, .. }) => Some(format!("bearer:{}", http_base_url.trim_end_matches('/'))),
            Some(Profile::Ssh { target, .. }) => Some(format!("ssh:{}", ssh_target_key(target))),
            None => None,
        }
    }
}

/// `insertRoute`: a new route goes after every saved route of the same or a faster kind, so LAN lands ahead of tailnet and both ahead
/// of T3 Connect.
pub fn insert_route(mut routes: Vec<Route>, route: Route) -> Vec<Route> {
    let rank = route.kind().rank();
    match routes.iter().position(|existing| existing.kind().rank() > rank) {
        Some(index) => routes.insert(index, route),
        None => routes.push(route),
    }
    routes
}

/// `upsertRoute`: replaces the route with the same id in place, or inserts it by kind.
pub fn upsert_route(mut routes: Vec<Route>, route: Route) -> Vec<Route> {
    match routes.iter().position(|existing| existing.id() == route.id()) {
        Some(index) => {
            routes[index] = route;
            routes
        }
        None => insert_route(routes, route),
    }
}

/// `findRouteToSameAddress`: registering an address again replaces that route, even when it was saved under another id.
pub fn find_route_to_same_address<'a>(routes: &'a [Route], route: &Route) -> Option<&'a Route> {
    let key = route.address_key()?;
    routes.iter().find(|existing| existing.address_key().as_ref() == Some(&key))
}

/// `routesAfterRemoving`: a learned route borrows the credential of the route it was learned over, so it cannot outlive that route.
pub fn routes_after_removing(routes: Vec<Route>, removed_id: &str) -> Vec<Route> {
    let Some(removed) = routes.iter().find(|route| route.id() == removed_id).cloned() else { return routes };
    routes
        .into_iter()
        .filter(|route| {
            if *route == removed {
                return false;
            }
            if !route.learned() {
                return true;
            }
            if matches!(&route.profile, Some(Profile::Bearer { authorization: Some(authorization), .. }) if authorization == "t3-connect") {
                return !matches!(removed.target, Target::Relay { .. });
            }
            credential_connection_id(&route.id()) != removed_id
        })
        .collect()
}

/// `sshTargetKey`: one SSH target as desktop keys its tunnels.
pub fn ssh_target_key(target: &SshTarget) -> String {
    serde_json::to_string(&(&target.alias, &target.hostname, &target.username, target.port)).expect("a tuple serializes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn id(value: &str) -> EnvironmentId {
        EnvironmentId::new(value).unwrap()
    }

    #[test]
    fn a_document_the_original_wrote_reads_and_writes_back_the_same() {
        let original = json!({
            "schemaVersion": 1,
            "targets": [
                {"_tag":"BearerConnectionTarget","environmentId":"env-a","label":"Home","connectionId":"bearer:env-a:http://192.168.1.4:3773"},
                {"_tag":"RelayConnectionTarget","environmentId":"env-a","label":"Home"},
                {"_tag":"SshConnectionTarget","environmentId":"env-b","label":"Box","connectionId":"ssh:env-b:x"}
            ],
            "profiles": [
                {"_tag":"BearerConnectionProfile","connectionId":"bearer:env-a:http://192.168.1.4:3773","environmentId":"env-a","label":"Home","httpBaseUrl":"http://192.168.1.4:3773/","wsBaseUrl":"ws://192.168.1.4:3773/"},
                {"_tag":"SshConnectionProfile","connectionId":"ssh:env-b:x","environmentId":"env-b","label":"Box","target":{"alias":"box","hostname":"box.example","username":null,"port":22}}
            ],
            "credentials": [{"connectionId":"bearer:env-a:http://192.168.1.4:3773","credential":{"_tag":"BearerConnectionCredential","token":"secret-token"}}],
            "remoteDpopTokens": [{"environmentId":"env-a","label":"Home","accessToken":"t","expiresAtEpochMs":1,"dpopThumbprint":"x","endpoint":{}}],
            "githubRoutingPermissions": [{"environmentId":"env-a","connectionKey":"k","permission":"off"}],
            "disabledEnvironmentIds": ["env-b"]
        });
        let document = Document::parse(&original.to_string()).unwrap();
        assert_eq!(serde_json::to_value(&document).unwrap(), original);
        // An older document predates the switched-off list.
        let older = Document::parse(r#"{"schemaVersion":1,"targets":[],"profiles":[],"credentials":[],"remoteDpopTokens":[]}"#).unwrap();
        assert_eq!(older, Document::default());
        assert!(Document::parse(r#"{"schemaVersion":2,"targets":[],"profiles":[],"credentials":[],"remoteDpopTokens":[]}"#).is_err());
        assert!(Document::parse("{").is_err());
    }

    #[test]
    fn a_secret_never_reaches_a_debug_print() {
        let mut document = Document::default();
        document.register(bearer_registration(&id("env-a"), "Home", "http://192.168.1.4:3773/", "secret-token"), None);
        assert!(!format!("{document:?}").contains("secret-token"));
    }

    #[test]
    fn registering_saves_the_route_its_profile_and_its_credential_and_forgetting_removes_them_all() {
        let mut document = Document::default();
        document.register(bearer_registration(&id("env-a"), "Home", "http://192.168.1.4:3773/", "tok-a"), None);
        document.register(bearer_registration(&id("env-b"), "Work", "https://work.example/", "tok-b"), None);
        let a = bearer_connection_id(&id("env-a"), "http://192.168.1.4:3773/");
        assert_eq!(a, "bearer:env-a:http://192.168.1.4:3773");
        assert_eq!((document.targets.len(), document.profiles.len(), document.token(&a)), (2, 2, Some("tok-a")));
        // Pairing the same address again replaces that route and its token, not adds one.
        document.register(bearer_registration(&id("env-a"), "Home renamed", "http://192.168.1.4:3773/", "tok-a2"), None);
        assert_eq!((document.targets.len(), document.profiles.len(), document.credentials.len(), document.token(&a)), (2, 2, 2, Some("tok-a2")));
        document.set_enabled(&id("env-a"), false);
        document.register(bearer_registration(&id("env-a"), "Home", "http://192.168.1.4:3773/", "tok-a3"), None);
        assert!(!document.enabled(&id("env-a")), "re-registering keeps the switched-off flag");
        document.remove(&id("env-a"));
        assert_eq!((document.targets.len(), document.profiles.len(), document.credentials.len(), document.disabled_environment_ids.len()), (1, 1, 1, 0));
        assert_eq!(document.token(&a), None);
    }

    #[test]
    fn setting_routes_drops_what_a_dropped_route_owned_and_keeps_the_environments_place() {
        let mut document = Document::default();
        document.register(bearer_registration(&id("env-a"), "A", "http://a.lan:1/", "tok-a"), None);
        document.register(bearer_registration(&id("env-b"), "B", "http://b.lan:1/", "tok-b"), None);
        document.register(bearer_registration(&id("env-c"), "C", "http://c.lan:1/", "tok-c"), None);
        let b = bearer_connection_id(&id("env-b"), "http://b.lan:1/");
        let relay = Target::Relay { environment_id: id("env-b"), label: "B".into() };
        document.set_routes(&id("env-b"), vec![relay.clone()]);
        let order: Vec<String> = document.entries().iter().map(|entry| entry.environment.to_string()).collect();
        assert_eq!(order, ["env-a", "env-b", "env-c"]);
        assert_eq!((document.token(&b), document.profile(&b)), (None, None));
        assert_eq!(document.routes_of(&id("env-b")), vec![&relay]);
    }

    #[test]
    fn routes_sort_by_how_fast_they_are_and_label_themselves() {
        let route = |url: &str| Route { target: Target::Bearer { environment_id: id("e"), label: "E".into(), connection_id: url.into() }, profile: Some(Profile::Bearer { connection_id: url.into(), environment_id: id("e"), label: "E".into(), http_base_url: url.into(), ws_base_url: String::new(), learned: None, authorization: None }) };
        let relay = Route { target: Target::Relay { environment_id: id("e"), label: "E".into() }, profile: None };
        let ssh = Route { target: Target::Ssh { environment_id: id("e"), label: "E".into(), connection_id: "ssh:1".into() }, profile: Some(Profile::Ssh { connection_id: "ssh:1".into(), environment_id: id("e"), label: "E".into(), target: SshTarget { alias: "box".into(), hostname: "box.example".into(), username: Some("me".into()), port: None } }) };
        let kinds: Vec<_> = ["http://127.0.0.1:3773/", "http://192.168.1.2:3773/", "https://pc.tail1234.ts.net/", "https://t3.example.com/"].iter().map(|url| route(url).kind()).collect();
        assert_eq!(kinds, [RouteKind::Loopback, RouteKind::Lan, RouteKind::Tailnet, RouteKind::Public]);
        let mut routes = vec![relay.clone()];
        for added in [route("https://t3.example.com/"), ssh.clone(), route("http://192.168.1.2:3773/"), route("https://pc.tail1234.ts.net/")] {
            routes = insert_route(routes, added);
        }
        let labels: Vec<String> = routes.iter().map(Route::label).collect();
        assert_eq!(labels, ["LAN", "Tailscale", "t3.example.com", "SSH me@box.example", "T3 Connect"]);
        assert_eq!(routes[3].address().as_deref(), Some("box"));
        assert_eq!(relay.address(), None);
        // An upsert replaces in place; the same address under another id is found.
        let replaced = upsert_route(routes.clone(), route("http://192.168.1.2:3773/"));
        assert_eq!(replaced, routes);
        let mut renamed = route("http://192.168.1.2:3773");
        renamed.target = Target::Bearer { environment_id: id("e"), label: "E".into(), connection_id: "other-id".into() };
        assert!(find_route_to_same_address(&routes, &renamed).is_some());
    }

    #[test]
    fn removing_a_route_takes_the_routes_learned_over_it() {
        let direct = bearer_registration(&id("e"), "E", "http://10.0.0.5:3773/", "tok");
        let Registration::Bearer { target, profile, .. } = direct else { unreachable!() };
        let paired = Route { target: target.clone(), profile: Some(profile) };
        let learned_id = format!("learned:e:http://10.0.0.6:3773@{}", paired.id());
        let learned = Route {
            target: Target::Bearer { environment_id: id("e"), label: "E".into(), connection_id: learned_id.clone() },
            profile: Some(Profile::Bearer { connection_id: learned_id.clone(), environment_id: id("e"), label: "E".into(), http_base_url: "http://10.0.0.6:3773/".into(), ws_base_url: String::new(), learned: Some(true), authorization: None }),
        };
        assert_eq!(credential_connection_id(&learned_id), paired.id());
        assert_eq!(routes_after_removing(vec![paired.clone(), learned.clone()], &paired.id()), vec![]);
        assert_eq!(routes_after_removing(vec![paired.clone(), learned.clone()], &learned_id), vec![paired.clone()]);
        // The learned route authenticates with the token of the route it came from.
        let mut document = Document::default();
        document.register(bearer_registration(&id("e"), "E", "http://10.0.0.5:3773/", "tok"), None);
        assert_eq!(document.token(&learned_id), Some("tok"));
    }
}
