//! The saved connections, kept between runs (`ConnectionProfileStore`/`ConnectionCredentialStore` of
//! `packages/client-runtime/src/connection/`, with the web and desktop stores of `apps/web/src/connection/storage.ts` and
//! `apps/desktop/src/app/DesktopConnectionCatalogStore.ts`). The document and its rules are `t3_client::catalog`; this holds it
//! in the model, writes it where the platform keeps it, and reads it back at start.
//!
//! Where it lives: the web build keeps the document in the browser's storage, as the original does; the desktop app keeps it
//! encrypted in a file under the app's data directory (`t3_desktop::catalog_vault`), the key being the operating system's. A
//! platform that cannot protect it saves nothing and says so. The document holds the bearer tokens of the environments the person
//! paired: nothing here puts a token in a signal, a log, a notice or the page, and a token is read at the moment a connection uses it.
use crate::{
    notice::{self, Level},
    runtime::{UiModel, UiModelStoreExt},
};
use dioxus::prelude::*;
use std::{cell::RefCell, rc::Rc};
use t3_client::{
    catalog::{self, Document, Registration},
    connection::EnvironmentEndpoint,
    github_routing,
};
use t3_contracts::EnvironmentId;

/// Why the stored document could not be read: the platform would not open it right now (nothing is wrong with the document), or the
/// document is not one this app can read (it is set aside, so that saving afresh does not destroy it).
#[cfg_attr(not(any(feature = "desktop", test)), allow(dead_code))]
pub(crate) enum ReadFailure {
    Unavailable(String),
    Unreadable(String),
}

/// Where the document is kept.
pub(crate) trait Backend {
    fn read(&self) -> Result<Option<String>, ReadFailure>;
    fn write(&self, document: &str) -> Result<(), String>;
    /// Keeps a document that could not be read, out of the way of the next save.
    fn set_aside(&self);
}

/// The browser's storage. The original keeps the same document in IndexedDB; the Rust app's other state is in `localStorage`, and so is this.
#[cfg(target_arch = "wasm32")]
struct BrowserStorage;
#[cfg(target_arch = "wasm32")]
impl BrowserStorage {
    const KEY: &'static str = "t3code:rust-connection-catalog:v1";
    fn storage() -> Result<web_sys::Storage, String> {
        web_sys::window().ok_or("Browser storage is unavailable.")?.local_storage().map_err(|_| "Browser storage is unavailable.")?.ok_or_else(|| "Browser storage is unavailable.".to_owned())
    }
}
#[cfg(target_arch = "wasm32")]
impl Backend for BrowserStorage {
    fn read(&self) -> Result<Option<String>, ReadFailure> {
        Self::storage().and_then(|storage| storage.get_item(Self::KEY).map_err(|_| "The saved connections could not be read.".to_owned())).map_err(ReadFailure::Unavailable)
    }
    fn write(&self, document: &str) -> Result<(), String> {
        Self::storage()?.set_item(Self::KEY, document).map_err(|_| "The browser would not keep the saved connections (its storage is full or blocked).".to_owned())
    }
    fn set_aside(&self) {
        if let Ok(storage) = Self::storage() {
            if let Ok(Some(raw)) = storage.get_item(Self::KEY) {
                let _ = storage.set_item(&format!("{}:corrupt:{}", Self::KEY, chrono::Utc::now().timestamp_millis()), &raw);
            }
            let _ = storage.remove_item(Self::KEY);
        }
    }
}

/// The desktop app's encrypted file.
#[cfg(any(feature = "desktop", test))]
pub(crate) struct Encrypted(pub t3_desktop::catalog_vault::Vault);
#[cfg(any(feature = "desktop", test))]
impl Backend for Encrypted {
    fn read(&self) -> Result<Option<String>, ReadFailure> {
        use t3_desktop::catalog_vault::ReadError;
        self.0.read().map_err(|error| match error {
            ReadError::Unavailable(message) => ReadFailure::Unavailable(message),
            ReadError::Unreadable(message) => ReadFailure::Unreadable(message),
        })
    }
    fn write(&self, document: &str) -> Result<(), String> {
        self.0.write(document)
    }
    fn set_aside(&self) {
        let _ = self.0.quarantine(chrono::Utc::now().timestamp_millis() as u128);
    }
}

/// The document in the model. Cloning shares it, as it does for the drafts.
#[derive(Clone, Default)]
pub struct Saved(Rc<RefCell<Inner>>);
#[derive(Default)]
struct Inner {
    backend: Option<Rc<dyn Backend>>,
    document: Document,
    /// Whether the stored document has been read (or failed to be): until it has, nothing is written over it.
    hydrated: bool,
}
impl std::fmt::Debug for Saved {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.0.borrow();
        formatter.debug_struct("Saved").field("environments", &inner.document.entries().len()).field("hydrated", &inner.hydrated).finish()
    }
}

impl Saved {
    /// A store kept in `backend`; the document is read from it by [`hydrate`].
    #[cfg_attr(not(any(test, feature = "desktop")), allow(dead_code))]
    pub(crate) fn in_backend(backend: Rc<dyn Backend>) -> Self {
        Self(Rc::new(RefCell::new(Inner { backend: Some(backend), ..Inner::default() })))
    }
    pub(crate) fn entries(&self) -> Vec<catalog::Entry> {
        self.0.borrow().document.entries()
    }
    pub(crate) fn entry(&self, environment: &EnvironmentId) -> Option<catalog::Entry> {
        self.entries().into_iter().find(|entry| &entry.environment == environment)
    }
    /// The token of a saved direct route, taken at the moment a connection uses it.
    pub(crate) fn token(&self, connection_id: &str) -> Option<String> {
        self.0.borrow().document.token(connection_id).map(str::to_owned)
    }
    /// What this client has granted other machines of each environment's GitHub access (`githubRoutingPermissions` of the document).
    pub(crate) fn github_grants(&self) -> Vec<github_routing::Stored> {
        let inner = self.0.borrow();
        inner.document.github_routing_permissions.iter().flatten().filter_map(|grant| serde_json::from_value(grant.clone()).ok()).collect()
    }
    fn write(&self) -> Result<(), String> {
        let inner = self.0.borrow();
        match (&inner.backend, inner.hydrated) {
            (Some(backend), true) => backend.write(&inner.document.encode()),
            (Some(_), false) => Err("The saved connections have not been read yet, so nothing is written over them.".into()),
            (None, _) => Err("This system has no secure place to keep sign-ins.".into()),
        }
    }
}

/// The platform's store.
fn default_backend() -> Option<Rc<dyn Backend>> {
    #[cfg(target_arch = "wasm32")]
    return Some(Rc::new(BrowserStorage));
    #[cfg(all(feature = "desktop", not(target_arch = "wasm32")))]
    return crate::desktop_shell::catalog_backend();
    #[cfg(not(any(feature = "desktop", target_arch = "wasm32")))]
    None
}

fn bump(state: Store<UiModel>) {
    let next = *state.saved_revision().peek() + 1;
    state.saved_revision().set(next);
}

fn report(state: Store<UiModel>, title: &str, cause: &str) {
    notice::push(state, Level::Error, title, cause);
}

/// Gives the model the platform's store, unless it was given one; called where the window's context is.
pub(crate) fn use_store(state: Store<UiModel>) {
    use_hook(|| {
        let saved = state.peek().saved.clone();
        if saved.0.borrow().backend.is_none() {
            saved.0.borrow_mut().backend = default_backend();
        }
    });
}

/// Reads the stored document and pins each saved address to the environment it was paired with. A document that cannot be read
/// is set aside and says so; one the platform would not open right now is left where it is.
pub(crate) async fn hydrate(mut state: Store<UiModel>) {
    let saved = state.peek().saved.clone();
    let backend = saved.0.borrow().backend.clone();
    let Some(backend) = backend else {
        saved.0.borrow_mut().hydrated = true;
        return;
    };
    let read = backend.read();
    let mut notice = None;
    match read {
        Ok(None) => saved.0.borrow_mut().hydrated = true,
        Ok(Some(raw)) => match Document::parse(&raw) {
            Ok(document) => {
                let mut inner = saved.0.borrow_mut();
                inner.document = document;
                inner.hydrated = true;
            }
            Err(cause) => {
                backend.set_aside();
                saved.0.borrow_mut().hydrated = true;
                notice = Some(format!("{cause} They were set aside, not deleted, and this app starts without them."));
            }
        },
        Err(ReadFailure::Unreadable(cause)) => {
            backend.set_aside();
            saved.0.borrow_mut().hydrated = true;
            notice = Some(format!("{cause} They were set aside, not deleted, and this app starts without them."));
        }
        // Not hydrated: a save would replace what could not be read.
        Err(ReadFailure::Unavailable(cause)) => notice = Some(cause),
    }
    if let Some(cause) = notice {
        report(state, "Saved connections", &cause);
    }
    let entries = saved.entries();
    {
        let mut model = state.write();
        for entry in &entries {
            for route in &entry.routes {
                if let Some(address) = route.http_base_url().and_then(|address| EnvironmentEndpoint::new(address).ok()) {
                    // A saved address that clashes with another saved identity keeps the first; the clash surfaces when it is used.
                    let _ = model.environments.remember(&address, entry.environment.clone(), entry.label.clone());
                }
            }
        }
    }
    bump(state);
}

/// Saves the route a connection was made over, with the token that opened it: pairing the same address again replaces its route, a
/// new address of a known environment is another route of it, the first of an unknown environment starts one.
pub(crate) fn remember(state: Store<UiModel>, address: &str, environment: &EnvironmentId, label: &str, token: &str) {
    if token.is_empty() {
        return;
    }
    let saved = state.peek().saved.clone();
    let registration = catalog::bearer_registration(environment, label, address, token);
    {
        let mut inner = saved.0.borrow_mut();
        let Registration::Bearer { target, profile, .. } = &registration else { unreachable!("a bearer registration") };
        // The routes the environment had stay, this one among them in the place its kind gives it.
        let existing = inner.document.entries().into_iter().find(|entry| &entry.environment == environment);
        let routes = existing.map(|entry| {
            let route = catalog::Route { target: target.clone(), profile: Some(profile.clone()) };
            let replaced = catalog::find_route_to_same_address(&entry.routes, &route).map(catalog::Route::id);
            let mut routes: Vec<catalog::Route> = entry.routes.into_iter().filter(|existing| Some(existing.id()) != replaced).collect();
            routes = catalog::upsert_route(routes, route);
            routes.into_iter().map(|route| route.target).collect::<Vec<_>>()
        });
        inner.document.register(registration, routes);
    }
    if let Err(cause) = saved.write() {
        report(state, "This connection was not saved", &format!("{cause} It works until you quit, and you will pair it again next time."));
    }
    bump(state);
}

/// Forgets an environment: its routes, profiles and tokens.
pub(crate) fn forget(state: Store<UiModel>, environment: &EnvironmentId) {
    let saved = state.peek().saved.clone();
    // An environment that was only seen, never saved, has nothing to remove.
    if saved.entry(environment).is_none() {
        return;
    }
    saved.0.borrow_mut().document.remove(environment);
    if let Err(cause) = saved.write() {
        report(state, "The saved connection was not removed", &cause);
    }
    bump(state);
}

/// Switches a saved environment off or on: off keeps its token and never connects it by itself.
pub(crate) fn set_enabled(state: Store<UiModel>, environment: &EnvironmentId, enabled: bool) {
    let saved = state.peek().saved.clone();
    if saved.entry(environment).is_none() {
        return;
    }
    saved.0.borrow_mut().document.set_enabled(environment, enabled);
    if let Err(cause) = saved.write() {
        report(state, "The saved connection was not changed", &cause);
    }
    bump(state);
}

// ---- GitHub sharing --------------------------------------------------------------------------------------------------------------

/// The endpoints an environment's trust is tied to: its saved routes (the ones the person paired, not those a server reported), or for
/// an environment the app only knows from where it was opened, that address.
fn github_endpoints(state: Store<UiModel>, environment: &EnvironmentId) -> Vec<github_routing::Endpoint> {
    let model = state.peek();
    let Some(entry) = model.saved.entry(environment) else {
        return model
            .environments
            .records
            .get(environment)
            .map(|record| github_routing::Endpoint::Primary { environment: environment.to_string(), http_base_url: record.address.clone(), ws_base_url: github_routing::ws_base_of(&record.address) })
            .into_iter()
            .collect();
    };
    entry
        .routes
        .iter()
        .filter(|route| !route.learned())
        .filter_map(|route| {
            let environment = environment.to_string();
            Some(match (&route.target, &route.profile) {
                (catalog::Target::Relay { .. }, _) => github_routing::Endpoint::Relay { environment },
                (catalog::Target::Bearer { .. }, Some(catalog::Profile::Bearer { http_base_url, ws_base_url, .. })) => github_routing::Endpoint::Bearer { environment, http_base_url: http_base_url.clone(), ws_base_url: ws_base_url.clone() },
                (catalog::Target::Ssh { .. }, Some(catalog::Profile::Ssh { target, .. })) => github_routing::Endpoint::Ssh { environment, alias: target.alias.clone(), hostname: target.hostname.clone(), username: target.username.clone(), port: target.port },
                // A route without its profile cannot be told from another, so it earns no trust.
                _ => return None,
            })
        })
        .collect()
}

/// The key the environment's grant is stored under; none while it has no endpoint to be tied to.
pub(crate) fn github_key(state: Store<UiModel>, environment: &EnvironmentId) -> Option<String> {
    github_routing::connection_key(&github_endpoints(state, environment))
}

/// What other machines may use of this environment's GitHub access, as this client has granted it (`gitHubRoutingPermissionFor`).
pub(crate) fn github_permission(state: Store<UiModel>, environment: &EnvironmentId) -> github_routing::Permission {
    github_routing::permission_for(github_key(state, environment).as_deref(), &state.peek().saved.github_grants())
}

/// Grants or withdraws an environment's GitHub access. Nothing changes when the grant cannot be kept.
pub(crate) fn set_github_permission(state: Store<UiModel>, environment: &EnvironmentId, permission: github_routing::Permission) -> Result<(), String> {
    let key = github_key(state, environment).ok_or("This environment does not have a saved connection endpoint.")?;
    let saved = state.peek().saved.clone();
    let next = github_routing::with_permission(&saved.github_grants(), environment.as_str(), &key, permission);
    let previous = std::mem::replace(&mut saved.0.borrow_mut().document.github_routing_permissions, Some(next.iter().filter_map(|grant| serde_json::to_value(grant).ok()).collect()));
    if let Err(cause) = saved.write() {
        saved.0.borrow_mut().document.github_routing_permissions = previous;
        return Err(cause);
    }
    bump(state);
    Ok(())
}

/// Connects every saved environment that is switched on and has a direct route and its token, each in the background: the window
/// stays on whatever it is on, and an environment that cannot be reached is tried again as any lost connection is (its routes in
/// the order the person keeps them).
pub(crate) fn connect_saved(transport: &crate::runtime::TransportHandle, state: Store<UiModel>) {
    let saved = state.peek().saved.clone();
    for entry in saved.entries().into_iter().filter(|entry| entry.enabled) {
        let routes: Vec<crate::runtime::Route> = entry
            .routes
            .iter()
            .filter_map(|route| Some(crate::runtime::Route { address: route.http_base_url()?.to_owned(), token: saved.token(route.target.connection_id()?)? }))
            .collect();
        if routes.is_empty() {
            continue;
        }
        let transport = transport.clone();
        let environment = entry.environment.clone();
        spawn(async move { crate::runtime::connect_routes_in_background(transport, state, routes, Some(environment)).await });
    }
}
