//! Several environments connected at once (`EnvironmentRegistry`, `packages/client-runtime/src/connection/registry.ts`): one link
//! per environment, each with its own socket, request ids, waiters and reconnect loop, and one slice of the model per link.
//!
//! The model keeps the flat fields it always had (`shell`, `thread`, `status`, `config`, ...) for the environment the window is on
//! (the active one), so every view keeps reading them as it did. The slice of every other connected environment is parked in
//! `UiModel::parked`, and the link's own tasks write there while it is parked: a frame on a link only ever lands in that link's
//! slice, and an inactive environment's shell keeps arriving without touching what the window shows. Switching the active
//! environment swaps the two, which loses nothing and asks the servers for nothing but the open thread.
//!
//! A [`TransportHandle`] without a pin follows the active link (what every view holds); one with a pin is that link for good, which
//! is what a link's own tasks and the requests it has sent hold, so an answer cannot be taken for another link's.
use super::{ConnectionStatus, EnvironmentEndpoint, EnvironmentId, PendingLaunch, PendingMessage, ShellState, ThreadState, Transport, UiModel, UiModelStoreExt, Value};
use dioxus::prelude::*;
use std::{
    cell::{Ref, RefCell, RefMut},
    collections::BTreeMap,
    rc::Rc,
};
use t3_contracts::{ServerConfig, SessionGrantInput};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct LinkId(u64);

/// The time the connections keep: what a supervisor measures with and sleeps on. A test sets one it moves by hand, so that an hour of
/// retries takes no time.
#[derive(Clone)]
pub(crate) struct Clock {
    /// Milliseconds on a clock that only goes forward.
    pub now: Rc<dyn Fn() -> u64>,
    pub sleep: Rc<dyn Fn(u64) -> futures_util::future::LocalBoxFuture<'static, ()>>,
}
impl Default for Clock {
    fn default() -> Self {
        Self {
            now: Rc::new(|| {
                #[cfg(target_arch = "wasm32")]
                return js_sys::Date::now() as u64;
                #[cfg(not(target_arch = "wasm32"))]
                {
                    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
                    START.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
                }
            }),
            sleep: Rc::new(|milliseconds| Box::pin(super::delay(milliseconds))),
        }
    }
}

#[derive(Default)]
struct Hub {
    clock: Clock,
    links: BTreeMap<LinkId, Transport>,
    active: Option<LinkId>,
    /// What an unpinned handle resolves to when no link is active, and a pinned one when its link is gone: nothing is connected.
    idle: Transport,
    next: u64,
}

impl Hub {
    fn resolve(&self, pin: Option<LinkId>) -> &Transport {
        pin.or(self.active).and_then(|id| self.links.get(&id)).unwrap_or(&self.idle)
    }
    fn resolve_mut(&mut self, pin: Option<LinkId>) -> &mut Transport {
        match pin.or(self.active).filter(|id| self.links.contains_key(id)) {
            Some(id) => self.links.get_mut(&id).expect("checked"),
            None => &mut self.idle,
        }
    }
}

/// The connections of this window. Cloning shares them.
#[derive(Clone, Default)]
pub struct TransportHandle {
    hub: Rc<RefCell<Hub>>,
    pin: Option<LinkId>,
}

impl PartialEq for TransportHandle {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.hub, &other.hub) && self.pin == other.pin
    }
}

impl TransportHandle {
    pub(crate) fn clock(&self) -> Clock {
        self.hub.borrow().clock.clone()
    }
    #[cfg(test)]
    pub(crate) fn set_clock(&self, clock: Clock) {
        self.hub.borrow_mut().clock = clock;
    }

    /// Every link, each as a handle of its own.
    #[cfg(test)]
    pub(crate) fn all(&self) -> Vec<Self> {
        self.hub.borrow().links.keys().map(|id| Self { hub: self.hub.clone(), pin: Some(*id) }).collect()
    }

    /// The link this handle is on: the active one, or the one it is pinned to.
    pub(crate) fn borrow(&self) -> Ref<'_, Transport> {
        Ref::map(self.hub.borrow(), |hub| hub.resolve(self.pin))
    }
    pub(crate) fn borrow_mut(&self) -> RefMut<'_, Transport> {
        RefMut::map(self.hub.borrow_mut(), |hub| hub.resolve_mut(self.pin))
    }

    /// This handle, pinned to the link it is on now.
    pub(crate) fn pinned(&self) -> Self {
        Self { hub: self.hub.clone(), pin: self.pin.or(self.hub.borrow().active) }
    }
    pub(crate) fn id(&self) -> Option<LinkId> {
        self.pin.or(self.hub.borrow().active)
    }

    /// A new link, or the one that already goes to this address (its tasks are superseded by the caller).
    pub(crate) fn open_link(&self, endpoint: Option<&EnvironmentEndpoint>) -> Self {
        let mut hub = self.hub.borrow_mut();
        if let Some(endpoint) = endpoint {
            if let Some((id, _)) = hub.links.iter().find(|(_, link)| link.endpoint.as_ref() == Some(endpoint)) {
                return Self { hub: self.hub.clone(), pin: Some(*id) };
            }
        }
        hub.next += 1;
        let id = LinkId(hub.next);
        // The address is the link's from the start, so that asking again for it (while it still cannot be reached) is the same link.
        hub.links.insert(id, Transport { endpoint: endpoint.cloned(), ..Transport::default() });
        Self { hub: self.hub.clone(), pin: Some(id) }
    }

    /// The link connected to this environment.
    pub(crate) fn link_of(&self, environment: &EnvironmentId) -> Option<Self> {
        let hub = self.hub.borrow();
        hub.links.iter().find(|(_, link)| link.environment.as_ref() == Some(environment)).map(|(id, _)| Self { hub: self.hub.clone(), pin: Some(*id) })
    }

    /// Ends a link: its socket closes, what waits on it is told, and it is no more.
    pub(crate) fn close(&self) {
        let Some(id) = self.id() else { return };
        let removed = {
            let mut hub = self.hub.borrow_mut();
            if hub.active == Some(id) {
                hub.active = None;
            }
            hub.links.remove(&id)
        };
        if let Some(mut link) = removed {
            if let Some(mut sender) = link.sender.take() {
                sender.close();
            }
            link.rpc.disconnect();
            link.fail_waiters();
        }
    }

    pub(crate) fn set_active(&self, link: Option<LinkId>) {
        self.hub.borrow_mut().active = link;
    }
    pub(crate) fn active_id(&self) -> Option<LinkId> {
        self.hub.borrow().active
    }
}

/// What is the model's for one environment: everything a view reads about the open environment that another environment must not
/// overwrite. The active environment's is the model's flat fields; the others' are parked.
#[derive(Debug, Clone, Default)]
pub struct EnvironmentState {
    pub status: ConnectionStatus,
    pub shell: ShellState,
    pub thread: ThreadState,
    pub active_thread: Option<String>,
    pub selected_project: Option<String>,
    pub config: Value,
    pub typed_config: Option<ServerConfig>,
    pub grants: SessionGrantInput,
    pub error: Option<String>,
    pub draft: String,
    pub active_draft: Option<String>,
    pub pending_messages: BTreeMap<String, PendingMessage>,
    pub pending_launches: BTreeMap<String, PendingLaunch>,
}

impl UiModel {
    /// The open environment's state, taken out of the flat fields (which are left empty).
    pub(crate) fn take_slice(&mut self) -> EnvironmentState {
        EnvironmentState {
            status: std::mem::take(&mut self.status),
            shell: std::mem::take(&mut self.shell),
            thread: std::mem::take(&mut self.thread),
            active_thread: self.active_thread.take(),
            selected_project: self.selected_project.take(),
            config: std::mem::take(&mut self.config),
            typed_config: self.typed_config.take(),
            grants: std::mem::take(&mut self.grants),
            error: self.error.take(),
            draft: std::mem::take(&mut self.draft),
            active_draft: self.active_draft.take(),
            pending_messages: std::mem::take(&mut self.pending_messages),
            pending_launches: std::mem::take(&mut self.pending_launches),
        }
    }
    /// The state of the environment the window is now on.
    pub(crate) fn put_slice(&mut self, slice: EnvironmentState) {
        self.status = slice.status;
        self.shell = slice.shell;
        self.thread = slice.thread;
        self.active_thread = slice.active_thread;
        self.selected_project = slice.selected_project;
        self.config = slice.config;
        self.typed_config = slice.typed_config;
        self.grants = slice.grants;
        self.error = slice.error;
        self.draft = slice.draft;
        self.active_draft = slice.active_draft;
        self.pending_messages = slice.pending_messages;
        self.pending_launches = slice.pending_launches;
    }
}

/// Where a link's state is now.
pub(crate) enum Slot {
    /// The link is the active one: the model's flat fields.
    Flat,
    /// Another environment is open: this one's slice is parked.
    Parked(EnvironmentId),
    /// The link has not learned which environment it is (a first attempt in the background), or is gone: it has no state yet.
    Nowhere,
}

pub(crate) fn slot(link: &TransportHandle) -> Slot {
    let hub = link.hub.borrow();
    // A handle that follows the window with nothing open is on the flat fields (which then hold nothing).
    let Some(id) = link.pin.or(hub.active) else { return Slot::Flat };
    if hub.active == Some(id) {
        return Slot::Flat;
    }
    match hub.links.get(&id).and_then(|link| link.environment.clone()) {
        Some(environment) => Slot::Parked(environment),
        None => Slot::Nowhere,
    }
}

/// The environment the link is connected to, from the slice it keeps its state in.
pub(crate) fn environment_of(link: &TransportHandle, state: Store<UiModel>) -> Option<EnvironmentId> {
    match slot(link) {
        Slot::Flat => state.peek().destination.clone(),
        Slot::Parked(environment) => Some(environment),
        Slot::Nowhere => None,
    }
}

pub(crate) fn status_of(link: &TransportHandle, state: Store<UiModel>) -> ConnectionStatus {
    match slot(link) {
        Slot::Flat => state.peek().status.clone(),
        Slot::Parked(environment) => state.peek().parked.get(&environment).map(|slice| slice.status.clone()).unwrap_or_default(),
        // A first attempt in the background has no environment to keep it for yet: the link keeps its own.
        Slot::Nowhere => link.borrow().status.clone(),
    }
}

/// Sets the link's status where it is kept, and in the list of every environment's status.
pub(crate) fn set_status(link: &TransportHandle, state: Store<UiModel>, status: ConnectionStatus) {
    match slot(link) {
        Slot::Flat => {
            let changed = *state.status().peek() != status;
            if changed {
                state.status().set(status.clone());
            }
            let environment = state.destination().peek().clone();
            if let Some(environment) = environment {
                state.live().write().insert(environment, status);
            }
        }
        Slot::Parked(environment) => {
            if let Some(slice) = state.parked().write().get_mut(&environment) {
                slice.status = status.clone();
            }
            state.live().write().insert(environment, status);
        }
        Slot::Nowhere => link.borrow_mut().status = status,
    }
}

/// The app-wide error banner for the active environment; an inactive one keeps its own until it is opened.
pub(crate) fn fail_on(link: &TransportHandle, state: Store<UiModel>, message: impl Into<String>) {
    let message = message.into();
    match slot(link) {
        Slot::Flat => state.error().set(Some(message)),
        Slot::Parked(environment) => {
            if let Some(slice) = state.parked().write().get_mut(&environment) {
                slice.error = Some(message);
            }
        }
        Slot::Nowhere => {}
    }
}

/// Leaves the open environment connected but not the window's: its state is parked and its thread stream (which only the open
/// environment keeps) is stopped. The window is left on nothing.
pub(crate) fn park_active(handle: &TransportHandle, mut state: Store<UiModel>) {
    if handle.active_id().is_some() {
        super::cancel_thread_subscription(&TransportHandle { hub: handle.hub.clone(), pin: None });
    }
    {
        let mut model = state.write();
        super::save_current_environment(&mut model);
        let slice = model.take_slice();
        if let Some(environment) = model.destination.take() {
            model.parked.insert(environment, slice);
        }
    }
    handle.set_active(None);
}

/// Makes the link the window's: the open environment is parked, and the link's own state (if it has any yet) takes the flat fields.
/// What the link kept, it kept; nothing is asked of its server here.
pub(crate) fn activate(link: &TransportHandle, mut state: Store<UiModel>) {
    if link.id().is_some() && link.id() == link.active_id() {
        return;
    }
    park_active(link, state);
    let environment = link.borrow().environment.clone();
    if let Some(environment) = environment {
        let mut model = state.write();
        if let Some(slice) = model.parked.remove(&environment) {
            model.put_slice(slice);
        }
        model.destination = Some(environment);
    }
    link.set_active(link.id());
    crate::draft_storage::restore_destination(state);
}

/// A new connection to an environment whose earlier one is lost takes its place, window and all: what the earlier one kept (its
/// shell, the thread that was open) stays the environment's.
pub(crate) fn take_over(new: &TransportHandle, other: &TransportHandle, mut state: Store<UiModel>) {
    let other_open = other.id().is_some() && other.id() == other.active_id();
    let new_open = new.id().is_some() && new.id() == new.active_id();
    let environment = other.borrow().environment.clone();
    other.close();
    if other_open {
        new.set_active(new.id());
    } else if new_open {
        if let Some(environment) = environment {
            let mut model = state.write();
            if let Some(slice) = model.parked.remove(&environment) {
                model.put_slice(slice);
            }
        }
    }
}
