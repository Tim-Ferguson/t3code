//! Reading every connected environment at once. The window shows one environment's state in the model's flat fields and keeps the
//! others' parked (`links`); the sidebar, the search, the palette and the notifications span all of them, as the original's do
//! (`useEnvironmentIds`, `environmentShell.stateValueAtom(environmentId)`). A row is named by its environment and its id, because
//! two servers can hold the same id.
//!
//! The readers here borrow the shells where they live, so nothing of a thread list is copied to be read.
use crate::{
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
    sidebar_rows::Caps,
};
use dioxus::prelude::*;
use std::collections::HashMap;
use t3_client::connection::ConnectionStatus;
use t3_contracts::{AuthEnvironmentScope, EnvironmentId, ShellSnapshot, ThreadShell};

/// What separates an environment from an id in a key that names a thread or a project in text (a palette value).
pub(crate) const SEPARATOR: char = ':';

/// One environment's shell.
pub(crate) struct Shell<'a> {
    pub environment: &'a EnvironmentId,
    pub snapshot: &'a ShellSnapshot,
}

/// Calls `read` with the shell of every connected environment that has one: the open environment's, and each parked one's, in the
/// order of their ids (so the sidebar does not rearrange itself when the window moves between them). The caller is subscribed to
/// all of them.
pub(crate) fn read<R>(state: Store<UiModel>, read: impl FnOnce(&[Shell<'_>]) -> R) -> R {
    let (destination, open, parked) = (state.destination(), state.shell(), state.parked());
    let (destination, open, parked) = (destination.read(), open.read(), parked.read());
    let mut shells: Vec<Shell<'_>> = parked.iter().filter_map(|(environment, slice)| slice.shell.snapshot.as_ref().map(|snapshot| Shell { environment, snapshot })).collect();
    if let (Some(environment), Some(snapshot)) = (destination.as_ref(), open.snapshot.as_ref()) {
        shells.push(Shell { environment, snapshot });
    }
    shells.sort_by(|left, right| left.environment.cmp(right.environment));
    read(&shells)
}

/// The environment of each thread of the shells, by where the thread lives: what lets a list of `&ThreadShell` that was ordered and
/// grouped by environment-blind code be told apart again.
pub(crate) struct Owners<'a>(HashMap<*const ThreadShell, &'a EnvironmentId>);
impl<'a> Owners<'a> {
    /// The owners of the threads of a listing (a row's threads), so a row does not index every thread of every environment.
    pub fn of(listed: &[(&'a EnvironmentId, &'a ThreadShell)]) -> Self {
        Self(listed.iter().map(|(environment, thread)| (*thread as *const ThreadShell, *environment)).collect())
    }
    pub fn get(&self, thread: &ThreadShell) -> Option<&'a EnvironmentId> {
        self.0.get(&(thread as *const ThreadShell)).copied()
    }
}

/// `environment:id`, the text that names a thread or a project of an environment (a palette value, a notification tag).
pub(crate) fn key(environment: &EnvironmentId, id: &str) -> String {
    format!("{environment}{SEPARATOR}{id}")
}
/// The environment and id a [`key`] names, among the environments that are connected.
pub(crate) fn split(state: Store<UiModel>, text: &str) -> Option<(EnvironmentId, String)> {
    let model = state.peek();
    model.live.keys().chain(model.destination.iter()).filter_map(|environment| text.strip_prefix(environment.as_str()).and_then(|rest| rest.strip_prefix(SEPARATOR)).map(|id| (environment.clone(), id.to_owned()))).max_by_key(|(environment, _)| environment.as_str().len())
}

/// The connection to an environment: its own link, so that what is sent and what answers is that environment's whichever one the
/// window is on. The window's own connection when there is no such link.
pub(crate) fn link(transport: &TransportHandle, environment: &EnvironmentId) -> TransportHandle {
    transport.link_of(environment).unwrap_or_else(|| transport.clone())
}

/// Whether the environment is the one the window is on.
pub(crate) fn is_open(state: Store<UiModel>, environment: &EnvironmentId) -> bool {
    state.destination().read().as_ref() == Some(environment)
}

/// The environment's connection status, whether it is open or parked.
pub(crate) fn status(state: Store<UiModel>, environment: &EnvironmentId) -> ConnectionStatus {
    if is_open(state, environment) {
        return state.status().read().clone();
    }
    state.live().read().get(environment).cloned().unwrap_or_default()
}

/// Whether the connection to the environment is up and its session may do what `scope` asks.
pub(crate) fn allows(state: Store<UiModel>, environment: &EnvironmentId, scope: AuthEnvironmentScope) -> bool {
    status(state, environment) == ConnectionStatus::Connected && state.environments().read().allows(environment, scope)
}

/// What the environment's server says it can do for threads. Read from the open environment's configuration, or the parked one's.
pub(crate) fn caps(state: Store<UiModel>, environment: &EnvironmentId) -> Caps {
    if is_open(state, environment) {
        return crate::sidebar_rows::caps(state);
    }
    let parked = state.parked();
    let parked = parked.read();
    crate::sidebar_rows::caps_from(parked.get(environment).and_then(|slice| slice.typed_config.as_ref()), state.client_settings().read().sidebar_working_shelf_enabled)
}

/// The label an environment is listed under.
pub(crate) fn label(state: Store<UiModel>, environment: &EnvironmentId) -> String {
    state.environments().read().records.get(environment).map_or_else(|| environment.to_string(), |record| record.label.clone())
}

/// How many environments are connected, the window's included.
pub(crate) fn count(state: Store<UiModel>) -> usize {
    state.live().read().len()
}

/// Makes the environment the window's, unless it is. False when it is not connected, in which case nothing changed.
pub(crate) fn open(transport: &TransportHandle, state: Store<UiModel>, environment: &EnvironmentId) -> bool {
    is_open(state, environment) || runtime::switch_to_environment(transport, state, environment)
}
