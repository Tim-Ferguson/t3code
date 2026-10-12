//! `t3 app` in the window: the page's half of the desktop app activation (`DesktopAppActivationCoordinator.tsx`). The shell serves the
//! control socket and holds each request until this is ready (`t3_desktop::app_activation`); this tells the broker when the app's own
//! environment can take a request, opens the project and a new thread in it, and answers.
//!
//! The environment a request opens in is the one this app started (or `T3_SERVER_URL` names), found by its address among the saved
//! environments; the window moves to it for the new thread, as the router of the original does.
use crate::runtime::{self, TransportHandle, UiModel, UiModelStoreExt};
use dioxus::prelude::*;
use futures_util::{FutureExt, StreamExt};
use std::{cell::RefCell, rc::Rc, sync::Arc};
use t3_client::{connection::{ConnectionStatus, EnvironmentEndpoint}, project_import::comparable, shell::ShellState, source_control::project_title};
use t3_contracts::{DesktopAppActivationRequest, EnvironmentId, ServerConfig};
use t3_desktop::app_activation::{Broker, Dependencies, Target, async_trait, handle_request};

/// How long a project that was added is waited for to reach the shell (`waitForProject`).
const PROJECT_WAIT_MS: u64 = 10_000;

/// What the shell gives the page: the broker the control socket feeds, and where this app's own server is.
#[derive(Clone)]
pub(crate) struct Handle {
    pub broker: Arc<Broker>,
    /// The address of the server this app started, once it answers; none while it does not.
    pub server_address: Rc<dyn Fn() -> Option<String>>,
}

type Waiter = (String, String, futures_channel::oneshot::Sender<()>);

/// The projects the page is waiting to see arrive, and the count that tells the effect which to look at.
#[derive(Clone)]
struct Waiting {
    list: Rc<RefCell<Vec<Waiter>>>,
    changes: Signal<u64>,
}

/// The environment whose address is the app's own.
pub(crate) fn primary(state: Store<UiModel>, address: &str) -> Option<EnvironmentId> {
    let wanted = EnvironmentEndpoint::new(address).ok()?.http("");
    let environments = state.environments();
    let catalog = environments.read();
    catalog.records.values().find(|record| EnvironmentEndpoint::new(&record.address).is_ok_and(|endpoint| endpoint.http("") == wanted)).map(|record| record.id.clone())
}

/// What the page knows of one environment, in the flat fields when the window is on it and parked when it is not.
fn with_slice<R>(state: Store<UiModel>, environment: &EnvironmentId, read: impl FnOnce(&ConnectionStatus, &ShellState, Option<&ServerConfig>) -> R) -> Option<R> {
    if state.destination().read().as_ref() == Some(environment) {
        let (status, shell, config) = (state.status(), state.shell(), state.typed_config());
        let (status, shell, config) = (status.read(), shell.read(), config.read());
        return Some(read(&status, &shell, config.as_ref()));
    }
    let parked = state.parked();
    let parked = parked.read();
    let slice = parked.get(environment)?;
    Some(read(&slice.status, &slice.shell, slice.typed_config.as_ref()))
}

/// `ready` of the coordinator: the environment is connected, its configuration is known and its shell has arrived.
fn is_ready(state: Store<UiModel>, address: Option<&str>) -> bool {
    let Some(environment) = address.and_then(|address| primary(state, address)) else { return false };
    with_slice(state, &environment, |status, shell, config| *status == ConnectionStatus::Connected && config.is_some() && shell.snapshot.is_some()).unwrap_or(false)
}

/// Lets go the waits for projects that the environment's shell now has.
fn release(waiting: &RefCell<Vec<Waiter>>, state: Store<UiModel>, environment: &EnvironmentId) {
    let known = with_slice(state, environment, |_, shell, _| shell.snapshot.as_ref().map(|snapshot| snapshot.projects.iter().map(|project| project.id.to_string()).collect::<Vec<_>>())).flatten().unwrap_or_default();
    let mut list = waiting.borrow_mut();
    let mut index = 0;
    while index < list.len() {
        if list[index].0 == environment.as_str() && known.contains(&list[index].1) {
            let (_, _, sender) = list.remove(index);
            let _ = sender.send(());
        } else {
            index += 1;
        }
    }
}

struct Page {
    state: Store<UiModel>,
    transport: TransportHandle,
    address: Rc<dyn Fn() -> Option<String>>,
    waiting: Waiting,
    /// Waits the milliseconds given; a test moves the time by hand.
    pause: Rc<dyn Fn(u64) -> futures_util::future::LocalBoxFuture<'static, ()>>,
}

impl Page {
    fn environment(&self) -> Option<EnvironmentId> {
        (self.address)().and_then(|address| primary(self.state, &address))
    }
}

#[async_trait(?Send)]
impl Dependencies for Page {
    fn target(&self) -> Option<Target> {
        let environment = self.environment()?;
        let (connected, platform) = with_slice(self.state, &environment, |status, _, config| (*status == ConnectionStatus::Connected, config.map(|config| config.environment.platform.os)))?;
        connected.then_some(())?;
        Some(Target { environment_id: environment.to_string(), platform: platform? })
    }

    fn find_project(&self, environment_id: &str, workspace_root: &str) -> Option<String> {
        let environment = EnvironmentId::new(environment_id).ok()?;
        let wanted = comparable(workspace_root);
        if wanted.is_empty() {
            return None;
        }
        with_slice(self.state, &environment, |_, shell, _| {
            shell.snapshot.as_ref()?.projects.iter().find(|project| comparable(project.workspace_root.as_str()) == wanted).map(|project| project.id.to_string())
        })?
    }

    async fn create_project(&self, environment_id: &str, workspace_root: &str) -> Result<String, String> {
        let environment = EnvironmentId::new(environment_id).map_err(|error| error.to_string())?;
        let link =self.transport.link_of(&environment).ok_or("The desktop app's primary local environment is not connected.")?;
        let project_id = uuid::Uuid::new_v4().to_string();
        let payload = serde_json::json!({"type":"project.create","commandId":uuid::Uuid::new_v4().to_string(),"projectId":project_id,"title":project_title(workspace_root),"workspaceRoot":workspace_root,"createWorkspaceRootIfMissing":false,"defaultModelSelection":null});
        runtime::request_value_reported(link, self.state, "projects.mutate", payload).await.map(|_| project_id)
    }

    async fn wait_for_project(&self, environment_id: &str, project_id: &str) -> Result<(), String> {
        let arrived = || {
            EnvironmentId::new(environment_id).ok().and_then(|environment| with_slice(self.state, &environment, |_, shell, _| shell.snapshot.as_ref().is_some_and(|snapshot| snapshot.projects.iter().any(|project| project.id.as_str() == project_id)))).unwrap_or(false)
        };
        if arrived() {
            return Ok(());
        }
        let (sender, receiver) = futures_channel::oneshot::channel();
        self.waiting.list.borrow_mut().push((environment_id.to_owned(), project_id.to_owned(), sender));
        let mut changes = self.waiting.changes;
        changes += 1;
        futures_util::select! {
            _ = receiver.fuse() => Ok(()),
            _ = (self.pause)(PROJECT_WAIT_MS).fuse() => {
                self.waiting.list.borrow_mut().retain(|(_, project, _)| project != project_id);
                Err("The project did not appear in the desktop app.".to_owned())
            }
        }
    }

    async fn open_thread(&self, environment_id: &str, project_id: &str) -> Result<Option<String>, String> {
        let environment = EnvironmentId::new(environment_id).map_err(|error| error.to_string())?;
        if !runtime::new_thread_in(&self.transport, self.state, &environment, Some(project_id.to_owned())) {
            return Ok(None);
        }
        // The thread of the original's draft is the one that will be started in it; this window's drafts are named apart from the
        // thread the server makes when one is sent, so the draft's own name is what the answer carries.
        let draft = self.state.active_draft().peek().clone();
        Ok(draft)
    }
}

/// Registers the page with the shell's broker while the app's own environment is ready, and opens each request that arrives, one
/// after the other. Nothing is drawn. The web build has no broker, and so nothing happens.
#[component]
pub(crate) fn Coordinator(state: Store<UiModel>, transport: TransportHandle) -> Element {
    let Some(handle) = try_consume_context::<Handle>() else { return rsx! {} };
    let ready = use_memo({
        let server_address = handle.server_address.clone();
        move || is_ready(state, server_address().as_deref())
    });
    let (sender, receiver) = use_hook(|| {
        let (sender, receiver) = futures_channel::mpsc::unbounded::<DesktopAppActivationRequest>();
        (sender, Rc::new(RefCell::new(Some(receiver))))
    });
    let waiting = Waiting { list: use_hook(Rc::default), changes: use_signal(|| 0) };

    // `appActivation.setReady`: the page can take requests while the environment is ready, and cannot when it is not.
    use_effect({
        let broker = handle.broker.clone();
        move || {
            if ready() {
                let sender = sender.clone();
                broker.register_renderer(Arc::new(move |request| sender.unbounded_send(request.clone()).map_err(|_| ())));
            } else {
                broker.clear_renderer();
            }
        }
    });
    // The window going away takes what was sent to it with it.
    use_drop({
        let broker = handle.broker.clone();
        move || broker.clear_renderer()
    });

    // Projects that were added are waited for until the shell has them.
    use_effect({
        let waiting = waiting.clone();
        let server_address = handle.server_address.clone();
        move || {
            let _ = (waiting.changes)();
            if waiting.list.borrow().is_empty() {
                return;
            }
            if let Some(environment) = server_address().and_then(|address| primary(state, &address)) {
                release(&waiting.list, state, &environment);
            }
        }
    });

    use_future({
        let (broker, server_address) = (handle.broker.clone(), handle.server_address.clone());
        move || {
            let (broker, server_address, transport, waiting) = (broker.clone(), server_address.clone(), transport.clone(), waiting.clone());
            let receiver = receiver.borrow_mut().take();
            async move {
                let Some(mut receiver) = receiver else { return };
                let page = Page { state, transport, address: server_address, waiting, pause: Rc::new(|milliseconds| Box::pin(runtime::delay(milliseconds))) };
                // One at a time: the next request waits until this one has opened its project.
                while let Some(request) = receiver.next().await {
                    let response = handle_request(&request, &page).await;
                    broker.complete(response);
                }
            }
        }
    });
    rsx! {}
}

#[cfg(test)]
#[path = "desktop_app_activation_tests.rs"]
mod tests;
