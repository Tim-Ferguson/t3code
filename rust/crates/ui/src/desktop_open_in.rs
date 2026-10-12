//! "Open in <editor>" for an environment on another machine, in the desktop app: the server there cannot start an editor for the person,
//! so the window hands the system a deep link and the editor on this computer connects over SSH (`remoteOpen.ts` and the remote branch of
//! `OpenInPicker.tsx`). The rules are `t3_desktop::remote_open`'s; this reads them off the window and puts them in the picker.
//!
//! The editors offered are the ones this computer has (`probeRemoteEditors`), and VS Code until it has said. The shell installs the
//! [`Bridge`]; a browser installs none and keeps the picker as it was.
use crate::{
    runtime::{UiModel, UiModelStoreExt},
    ssh_environments::Active,
};
use dioxus::prelude::*;
use futures_util::future::LocalBoxFuture;
use std::{cell::RefCell, rc::Rc};
use t3_desktop::remote_open::{self, HostKind, State};

/// What the desktop shell gives the picker.
#[derive(Clone)]
pub(crate) struct Bridge {
    /// The address of the server this app runs for itself, once it answers; none while it does not.
    pub server_address: Rc<dyn Fn() -> Option<String>>,
    /// Hands a link to the system. False when the system refused it (`openExternal`).
    pub open_link: Rc<dyn Fn(&str) -> bool>,
    /// The ids of the remote-capable editors installed on this computer (`probeRemoteEditors`).
    pub probe: Rc<dyn Fn() -> LocalBoxFuture<'static, Vec<String>>>,
}

thread_local! {
    /// The window's one thread is the only one that reads these.
    static BRIDGE: RefCell<Option<Bridge>> = const { RefCell::new(None) };
    /// The environment of the app's own server, as last seen.
    static OWN_ENVIRONMENT: RefCell<Option<t3_contracts::EnvironmentId>> = const { RefCell::new(None) };
}

pub(crate) fn install(bridge: Bridge) {
    OWN_ENVIRONMENT.with(|last| *last.borrow_mut() = None);
    BRIDGE.with(|slot| *slot.borrow_mut() = Some(bridge));
}

fn bridge() -> Option<Bridge> {
    BRIDGE.with(|slot| slot.borrow().clone())
}

/// `cachedProbedEditors`: what this computer has, once it has been asked. The answer is kept for as long as the app runs.
static PROBED: GlobalSignal<Option<Vec<String>>> = Signal::global(|| None);
/// `useRemoteOpenHint`: whether the first remote open has happened.
static HINT_SEEN: GlobalSignal<bool> = Signal::global(|| false);

/// The browser's key for the hint having been seen, and how it is stored (a JSON boolean, as the original's schema-checked storage).
const HINT_KEY: &str = "t3code:remote-open-hint-seen";

fn read_hint() {
    let key = serde_json::to_string(HINT_KEY).expect("key");
    spawn(async move {
        let stored = dioxus::document::eval(&format!("try{{return window.localStorage.getItem({key});}}catch{{return null;}}")).join::<Option<String>>().await.ok().flatten();
        if stored.and_then(|text| serde_json::from_str::<bool>(&text).ok()) == Some(true) {
            *HINT_SEEN.write() = true;
        }
    });
}
fn write_hint() {
    *HINT_SEEN.write() = true;
    let key = serde_json::to_string(HINT_KEY).expect("key");
    dioxus::document::eval(&format!("try{{window.localStorage.setItem({key},'true');}}catch{{}}"));
}

/// How the window opens an editor now, or none when this is not the desktop app (the picker then works as it did). An environment
/// whose server cannot be told apart from the app's own (it has not answered yet) is the app's.
pub(crate) fn mode(state: Store<UiModel>) -> Option<State> {
    let bridge = bridge()?;
    let destination = state.destination().read().clone()?;
    // While the app's server restarts it has no address; the environment it was is still this machine's.
    let own = (bridge.server_address)().and_then(|address| crate::desktop_app_activation::primary(state, &address));
    if let Some(own) = &own {
        OWN_ENVIRONMENT.with(|last| *last.borrow_mut() = Some(own.clone()));
    }
    let own_server = own.or_else(|| OWN_ENVIRONMENT.with(|last| last.borrow().clone())).as_ref() == Some(&destination);
    // The alias of the environment the window was switched to over SSH.
    let alias = state.ssh().read().active.clone().filter(|Active { environment, .. }| *environment == destination).map(|active| active.target.alias);
    let advertised: Vec<(HostKind, String)> = state
        .typed_config()
        .read()
        .as_ref()
        .and_then(|config| config.remote_open_targets.as_ref())
        .map(|targets| {
            targets
                .0
                .iter()
                .map(|target| (if target.kind == t3_contracts::RemoteOpenTargetKind::Tailscale { HostKind::Tailscale } else { HostKind::Mdns }, target.host.to_string()))
                .collect()
        })
        .unwrap_or_default();
    Some(remote_open::resolve_state(&remote_open::Environment { own_server, ssh_alias: alias.as_deref(), advertised: &advertised }))
}

/// The editors to offer in place of the server's, when the window is on another machine: those of this computer that take a link, or none
/// when there is no way to reach that machine. None for an environment the server starts editors for.
pub(crate) fn editors(state: Store<UiModel>) -> Option<Vec<String>> {
    match mode(state)? {
        State::LocalExec => None,
        State::RemoteUnavailable => Some(Vec::new()),
        State::RemoteLinks(_) => Some(PROBED.read().clone().unwrap_or_else(|| remote_open::FALLBACK_EDITORS.map(str::to_owned).to_vec())),
    }
}

/// Whether an editor may be opened: on another machine by a link, which needs no permission of the server's; else as the server asks.
pub(crate) fn links(state: Store<UiModel>) -> bool {
    matches!(mode(state), Some(State::RemoteLinks(_)))
}

/// Asks this computer which editors it has, once, and reads whether the hint was seen. Called by the picker.
pub(crate) fn use_probe() {
    use_hook(|| {
        let Some(bridge) = bridge() else { return };
        read_hint();
        if PROBED.peek().is_none() {
            let probing = (bridge.probe)();
            spawn(async move {
                let found: Vec<String> = probing.await.into_iter().filter(|id| remote_open::remote_scheme(id).is_some()).collect();
                *PROBED.write() = Some(if found.is_empty() { remote_open::FALLBACK_EDITORS.map(str::to_owned).to_vec() } else { found });
            });
        }
    });
}

/// `openInEditor` in remote-link mode: the link for `path` on the environment's machine, handed to the system. None when the environment
/// is one whose server starts editors. Some(true) when the system took the link; the hint of the first remote open is then done.
pub(crate) fn open(state: Store<UiModel>, path: &str, editor: &str) -> Option<bool> {
    let host = match mode(state)? {
        State::LocalExec => return None,
        State::RemoteUnavailable => return Some(false),
        State::RemoteLinks(host) => host,
    };
    let url = remote_open::build_remote_open_url(editor, &host.host, path)?;
    let opened = (bridge()?.open_link)(&url);
    if opened {
        write_hint();
    }
    Some(opened)
}

/// What the picker says in place of an editor when no name reaches the machine (`No SSH route to <environment>`).
pub(crate) fn no_route(state: Store<UiModel>) -> Option<String> {
    if mode(state)? != State::RemoteUnavailable {
        return None;
    }
    let destination = state.destination().read().clone()?;
    let label = state.environments().read().records.get(&destination).map(|record| record.label.clone()).unwrap_or_else(|| "this machine".to_owned());
    Some(format!("No SSH route to {label}"))
}

/// The note that stays in the menu until the first remote open (`Opens over SSH. Needs your key on <environment>`).
pub(crate) fn hint(state: Store<UiModel>) -> Option<String> {
    if !links(state) || *HINT_SEEN.read() {
        return None;
    }
    let destination = state.destination().read().clone()?;
    let label = state.environments().read().records.get(&destination).map(|record| record.label.clone()).unwrap_or_else(|| "this machine".to_owned());
    Some(format!("Opens over SSH. Needs your key on {label}"))
}

#[cfg(test)]
#[path = "desktop_open_in_tests.rs"]
mod tests;
