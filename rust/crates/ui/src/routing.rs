//! The address bar of the web target: the address follows the view and the open thread, and the view
//! follows the address on load and on back and forward (`t3_client::route` has the paths). It is a leaf
//! component that reads only the view, the open thread and the environment, so changing the address
//! rebuilds nothing else, and the page script (`assets/routing_abi.js`) is the only part that touches
//! the browser. The desktop window has no address bar and does not mount it.
use crate::{
    notice::{self, Level},
    runtime::{self, UiModel, UiModelStoreExt, View},
};
use dioxus::prelude::*;
use serde_json::{Value, json};
use t3_client::{
    connection::ConnectionStatus,
    route::{self, Route, SettingsPage},
};
use t3_contracts::EnvironmentId;

/// Switches the router on where the page has an address bar of its own: tests provide it.
#[derive(Clone, Copy)]
pub(crate) struct Enabled;

pub(crate) fn enabled() -> bool {
    cfg!(target_arch = "wasm32") || try_consume_context::<Enabled>().is_some()
}

fn page_view(page: SettingsPage) -> View {
    match page {
        SettingsPage::General => View::General,
        SettingsPage::Providers => View::Providers,
        SettingsPage::Appearance => View::Appearance,
        SettingsPage::Connections => View::Connections,
        SettingsPage::Keybindings => View::Keybindings,
        SettingsPage::ScheduledTasks => View::ScheduledTasks,
        SettingsPage::SourceControl => View::SourceControl,
        SettingsPage::TextGeneration => View::TextGeneration,
        SettingsPage::Projects => View::Projects,
        SettingsPage::Diagnostics => View::Diagnostics,
        SettingsPage::Storage => View::Storage,
        SettingsPage::Archived => View::Archived,
    }
}

/// The route the app is showing.
pub(crate) fn route_of(view: &View, thread: Option<&str>, environment: Option<&EnvironmentId>) -> Route {
    let page = |page| Route::Settings(page);
    match view {
        View::Chat => match (thread, environment) {
            (Some(thread), Some(environment)) => Route::Thread { environment: environment.to_string(), thread: thread.to_owned() },
            _ => Route::Start,
        },
        View::General => page(SettingsPage::General),
        View::Providers => page(SettingsPage::Providers),
        View::Appearance => page(SettingsPage::Appearance),
        View::Connections => page(SettingsPage::Connections),
        View::Keybindings => page(SettingsPage::Keybindings),
        View::ScheduledTasks => page(SettingsPage::ScheduledTasks),
        View::SourceControl => page(SettingsPage::SourceControl),
        View::TextGeneration => page(SettingsPage::TextGeneration),
        View::Projects => page(SettingsPage::Projects),
        View::Diagnostics => page(SettingsPage::Diagnostics),
        View::Storage => page(SettingsPage::Storage),
        View::Archived => page(SettingsPage::Archived),
        View::PullRequests => Route::PullRequests,
        View::Usage => Route::Usage,
    }
}

/// Whether the route can be shown now: a thread needs the server's list of threads.
/// Opens the Projects settings on the project a key names: a project id, or an environment id and a project id joined by a colon.
fn open_project_settings(state: Store<UiModel>, key: &str) {
    let project = state.peek().shell.snapshot.as_ref().and_then(|snapshot| snapshot.projects.iter().find(|project| key == project.id.as_str() || key.rsplit_once(':').is_some_and(|(_, id)| id == project.id.as_str())).map(|project| project.id.to_string()));
    if let Some(project) = project {
        state.settings_project().set(Some(project));
    }
    if *state.view().peek() != View::Projects {
        state.view().set(View::Projects);
    }
}

fn needs_data(route: &Route) -> bool {
    matches!(route, Route::Thread { .. } | Route::Project(_))
}

/// Shows the view an address names. A thread that is not on this server (or not in this environment)
/// falls back to the start screen with a notice, and the address is rewritten to say so.
fn apply(transport: &runtime::TransportHandle, state: Store<UiModel>, page: Option<document::Eval>, address: &str) {
    let replace = |path: &str| {
        if let Some(eval) = page {
            let _ = eval.send(json!({"type":"replace","path":path}));
        }
    };
    let start = |transport: &runtime::TransportHandle| {
        let model = state.peek();
        if model.active_thread.is_some() || model.view != View::Chat {
            drop(model);
            runtime::new_thread(transport, state, None);
        }
    };
    let Some(wanted) = route::parse(address) else {
        start(transport);
        replace("/");
        return;
    };
    match wanted {
        Route::Start => start(transport),
        // The original redirects `/projects/<key>` to the project's settings and rewrites the address; so does this. The key is a
        // project id, or an environment id and a project id joined by a colon.
        Route::Project(key) => {
            open_project_settings(state, &key);
            replace("/settings/projects");
        }
        Route::Settings(SettingsPage::Projects) if route::project_query(address).is_some() => {
            open_project_settings(state, &route::project_query(address).unwrap_or_default());
        }
        Route::Settings(settings) => {
            let view = page_view(settings);
            if *state.view().peek() != view {
                state.view().set(view);
            }
        }
        Route::PullRequests | Route::Usage => {
            let view = if wanted == Route::Usage { View::Usage } else { View::PullRequests };
            if *state.view().peek() != view {
                state.view().set(view);
            }
        }
        Route::Thread { environment, thread } => {
            let (present, open) = {
                let model = state.peek();
                let here = model.destination.as_ref().is_some_and(|destination| destination.to_string() == environment);
                let present = here && model.shell.snapshot.as_ref().is_some_and(|snapshot| snapshot.threads.iter().any(|candidate| candidate.id.as_str() == thread && candidate.deleted_at.is_none()));
                (present, model.active_thread.as_deref() == Some(thread.as_str()) && model.view == View::Chat)
            };
            if !present {
                start(transport);
                notice::push(state, Level::Warning, "Thread not found", "That address names a thread this server does not have. Showing the start screen.");
                replace("/");
            } else if !open {
                // Back from a settings page to the thread that was open: its conversation is still the server's
                // current word, so only the view changes, as the page's own way back does.
                let here = {
                    let model = state.peek();
                    model.active_thread.as_deref() == Some(thread.as_str()) && model.thread.synchronized
                };
                if here {
                    state.view().set(View::Chat);
                } else {
                    runtime::select_thread(transport, state, thread);
                }
            }
        }
    }
}

/// Keeps the address and the view in step. Mounted once by the application when `enabled()`.
#[component]
pub(crate) fn Router(state: Store<UiModel>, transport: runtime::TransportHandle) -> Element {
    let mut page = use_signal(|| None::<document::Eval>);
    // An address that waits for the server's threads, and whether the first address has been shown:
    // until it has, the address is the person's and the app does not write over it.
    let mut waiting = use_signal(|| None::<String>);
    let mut loaded = use_signal(|| false);
    let ready = use_memo(move || {
        let synchronized = state.shell().read().synchronized && state.shell().read().snapshot.is_some();
        synchronized && state.destination().read().is_some() && *state.status().read() == ConnectionStatus::Connected
    });
    let reading = transport.clone();
    use_future(move || {
        let transport = reading.clone();
        async move {
            let mut eval = document::eval(include_str!("../assets/routing_abi.js"));
            page.set(Some(eval));
            while let Ok(message) = eval.recv::<Value>().await {
                let Some(address) = message["path"].as_str().filter(|_| message["type"] == "location") else { continue };
                if route::parse(address).is_some_and(|wanted| needs_data(&wanted)) && !*ready.peek() {
                    waiting.set(Some(address.to_owned()));
                } else {
                    waiting.set(None);
                    apply(&transport, state, Some(eval), address);
                    loaded.set(true);
                }
            }
        }
    });
    let waiting_transport = transport.clone();
    use_effect(move || {
        if !ready() {
            return;
        }
        let Some(address) = waiting.peek().clone() else { return };
        waiting.set(None);
        apply(&waiting_transport, state, *page.peek(), &address);
        loaded.set(true);
    });
    use_effect(move || {
        let view = state.view().read().clone();
        let thread = state.active_thread().read().clone();
        let environment = state.destination().read().clone();
        if !*loaded.read() {
            return;
        }
        let path = route::path(&route_of(&view, thread.as_deref(), environment.as_ref()));
        if let Some(eval) = *page.peek() {
            let _ = eval.send(json!({"type":"push","path":path}));
        }
    });
    use_drop(move || {
        if let Some(eval) = *page.peek() {
            let _ = eval.send(json!({"type":"stop"}));
        }
    });
    rsx! {}
}
