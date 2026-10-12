//! The tabbed panel beside the conversation (`RightPanelTabs.tsx`, `rightPanelStore.ts`, and the panel half of `ChatView.tsx`):
//! one panel per thread holding the Diff, Files, file, pull request and linked pull requests tabs, a tab bar with the add
//! menu and the menu of a tab, the launcher an empty panel shows, hiding and maximizing the panel, and the history that brings a
//! closed tab back. The rules are `t3_client::right_panel`; the panels' own bodies stay in their modules.
//!
//! The model keeps the panel of each thread (and of the new-thread form, which can open files) in the browser's storage, so
//! a reload finds the same tabs. What the panel draws is derived from one thread's entry behind a memo, so the conversation and
//! the sidebar never repaint for it, and a streaming turn never repaints an open panel.
//!
//! The browser tab is `preview_panel`, the device tab `device_panel`; a terminal tab holds one of the thread's terminals (the drawer under
//! the conversation holds the others, and the splits). Closing a browser or terminal tab also closes the session or the terminal
//! behind it, which is done once the tab is gone (`use_cleanup`).
use crate::{
    device_panel, diff_panel, file_panel, pull_request_panel,
    notice::{self, Level},
    preview_panel,
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
    terminal_pane,
    thread_pull_requests_panel,
};
use dioxus::prelude::*;
use t3_client::{
    connection::ConnectionStatus,
    right_panel::{self as logic, Closed, History, Kind, Panels, PullRequestTarget, Surface, ThreadPanel},
};
use t3_contracts::AuthEnvironmentScope;

/// The owner a panel is kept under when no thread is open: the new-thread form, which can open files of its project.
pub(crate) const NEW_THREAD_FORM: &str = "~new";

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Model {
    pub panels: Panels,
    pub history: History,
    /// The owner whose panel fills the window (`maximizedRightPanelThreadKey`).
    pub maximized: Option<String>,
    /// The add menu of the open panel is showing (`rightPanel.new` opens it).
    pub menu: bool,
    /// Storage has been read: nothing is written before, so the first read is not overwritten.
    pub hydrated: bool,
    /// Tabs that have gone and still have a browser session or a terminal to close on the environment.
    pub cleanup: Vec<(Owner, Surface)>,
}

/// Who a panel belongs to.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Owner {
    pub environment: String,
    pub thread: Option<String>,
}
impl Owner {
    pub fn key(&self) -> String {
        key_of(&self.environment, self.thread.as_deref())
    }
}
pub(crate) fn key_of(environment: &str, thread: Option<&str>) -> String {
    format!("{environment}:{}", thread.unwrap_or(NEW_THREAD_FORM))
}
/// The owner of the panel in view: the open thread, else the new-thread form.
pub(crate) fn active_owner(state: Store<UiModel>) -> Option<Owner> {
    let environment = state.destination().peek().as_ref()?.to_string();
    Some(Owner { environment, thread: state.active_thread().peek().clone() })
}

// ---- storage ------------------------------------------------------------------------------

#[cfg(test)]
thread_local! {
    /// What the browser's storage holds under test: nothing here touches a browser.
    pub(crate) static MEMORY: std::cell::RefCell<std::collections::BTreeMap<String, String>> = const { std::cell::RefCell::new(std::collections::BTreeMap::new()) };
}
async fn read(key: &str) -> Option<String> {
    #[cfg(test)]
    {
        return MEMORY.with(|memory| memory.borrow().get(key).cloned());
    }
    #[cfg(all(not(test), target_arch = "wasm32"))]
    {
        return web_sys::window()?.local_storage().ok().flatten()?.get_item(key).ok().flatten();
    }
    #[cfg(all(not(test), not(target_arch = "wasm32")))]
    {
        let key = serde_json::to_string(key).expect("key");
        document::eval(&format!("try{{return window.localStorage.getItem({key});}}catch{{return null;}}")).join::<Option<String>>().await.ok().flatten()
    }
}
async fn write(key: &str, raw: &str) {
    #[cfg(test)]
    {
        MEMORY.with(|memory| memory.borrow_mut().insert(key.to_owned(), raw.to_owned()));
    }
    #[cfg(all(not(test), target_arch = "wasm32"))]
    {
        if let Some(storage) = web_sys::window().and_then(|window| window.local_storage().ok().flatten()) {
            let _ = storage.set_item(key, raw);
        }
    }
    #[cfg(all(not(test), not(target_arch = "wasm32")))]
    {
        let (key, value) = (serde_json::to_string(key).expect("key"), serde_json::to_string(raw).expect("value"));
        let _ = document::eval(&format!("try{{window.localStorage.setItem({key},{value});return true;}}catch{{return false;}}")).join::<bool>().await;
    }
}

/// Reads the panels and the closed tabs once, when the app starts.
pub async fn hydrate(state: Store<UiModel>) {
    let panels = Panels::parse(read(logic::STORAGE_KEY).await.as_deref());
    let history = History::parse(read(logic::CLOSED_STORAGE_KEY).await.as_deref());
    let mut field = state.right_panel();
    let mut model = field.write();
    model.panels = panels;
    model.history = history;
    model.hydrated = true;
}

/// Writes the panels and the history when they change. A write is a short string and the browser's own; nothing waits for it.
pub(crate) fn use_persist(state: Store<UiModel>) {
    let panels = use_memo(move || state.right_panel().read().panels.to_json());
    let history = use_memo(move || state.right_panel().read().history.to_json());
    use_effect(move || {
        let (panels, history) = (panels(), history());
        if !state.right_panel().peek().hydrated {
            return;
        }
        spawn(async move {
            write(logic::STORAGE_KEY, &panels).await;
            write(logic::CLOSED_STORAGE_KEY, &history).await;
        });
    });
}

// ---- reading ------------------------------------------------------------------------------

fn peek<R>(state: Store<UiModel>, read: impl FnOnce(&Model) -> R) -> R {
    let field = state.right_panel();
    let model = field.peek();
    read(&model)
}
fn edit<R>(state: Store<UiModel>, change: impl FnOnce(&mut Model) -> R) -> R {
    let mut field = state.right_panel();
    let mut model = field.write();
    change(&mut model)
}

/// What the panel of the open thread shows, subscribing the caller.
pub(crate) fn shown_surface(state: Store<UiModel>) -> Option<Surface> {
    let environment = state.destination().read().as_ref()?.to_string();
    let key = key_of(&environment, state.active_thread().read().as_deref());
    state.right_panel().read().panels.active(&key).cloned()
}
/// Whether the open thread's panel is open on `kind`, subscribing the caller.
pub(crate) fn is_shown(state: Store<UiModel>, kind: Kind) -> bool {
    shown_surface(state).is_some_and(|surface| surface.kind() == Some(kind))
}
/// Whether the open thread's panel is open at all (the `rightPanelOpen` of a shortcut's `when`), subscribing the caller.
pub(crate) fn is_open(state: Store<UiModel>) -> bool {
    let Some(environment) = state.destination().read().as_ref().map(ToString::to_string) else { return false };
    let key = key_of(&environment, state.active_thread().read().as_deref());
    state.right_panel().read().panels.get(&key).is_open
}
/// The shown tab of the open thread's panel, without subscribing the caller.
pub(crate) fn shown_peek(state: Store<UiModel>) -> Option<Surface> {
    let owner = active_owner(state)?;
    peek(state, |model| model.panels.active(&owner.key()).cloned())
}
pub(crate) fn shown_now(state: Store<UiModel>, kind: Kind) -> bool {
    shown_peek(state).is_some_and(|surface| surface.kind() == Some(kind))
}
/// Whether there is a closed tab to bring back, subscribing the caller.
pub(crate) fn can_reopen(state: Store<UiModel>) -> bool {
    !state.right_panel().read().history.entries.is_empty()
}

// ---- changing -----------------------------------------------------------------------------

fn remember(state: Store<UiModel>, owner: &Owner, removed: Vec<Surface>) {
    // The new-thread form has no thread to come back to.
    let Some(thread) = owner.thread.as_deref() else { return };
    edit(state, |model| {
        for surface in removed {
            // A session or a terminal is closed once the tab is gone, and a browser tab with a session is remembered from there.
            if matches!(surface, Surface::Preview { resource_id: Some(_) } | Surface::Terminal { .. }) {
                model.cleanup.push((owner.clone(), surface));
                continue;
            }
            model.history.remember(&owner.environment, thread, surface, uuid::Uuid::new_v4().to_string());
        }
    });
}

/// Closes what the tabs that went held: the browser session (and remembers it for reopening) and the terminal. Mounted with the app,
/// since the panel is gone when the last tab is.
pub(crate) fn use_cleanup(state: Store<UiModel>, transport: TransportHandle) {
    let pending = use_memo(move || !state.right_panel().read().cleanup.is_empty());
    use_effect(move || {
        if !pending() {
            return;
        }
        let gone = std::mem::take(&mut state.right_panel().write().cleanup);
        for (owner, surface) in gone {
            let Some(thread) = owner.thread.clone() else { continue };
            // The tabs of another environment than the one in view are closed there when the window is there again; none are.
            if state.destination().peek().as_ref().map(ToString::to_string).as_deref() != Some(owner.environment.as_str()) {
                continue;
            }
            match surface {
                Surface::Preview { resource_id: Some(tab) } => {
                    if let Some(closed) = preview_panel::closed_browser(state, &owner.environment, &thread, &tab) {
                        edit(state, |model| model.history.remember_browser(&owner.environment, &thread, Surface::Preview { resource_id: Some(tab.clone()) }, closed, uuid::Uuid::new_v4().to_string()));
                    }
                    preview_panel::close_session(&transport, state, &owner.environment, &thread, &tab);
                }
                Surface::Terminal { terminal_id } => terminal_pane::close_on_server(&transport, state, &thread, &terminal_id),
                _ => {}
            }
        }
    });
}

/// Shows a tab that exists once, in this owner's panel.
pub(crate) fn open_kind_in(state: Store<UiModel>, owner: &Owner, kind: Kind) {
    edit(state, |model| model.panels.open(&owner.key(), kind));
}
#[cfg(test)]
pub(crate) fn open_kind(state: Store<UiModel>, kind: Kind) {
    if let Some(owner) = active_owner(state) {
        open_kind_in(state, &owner, kind);
    }
}
/// Shows `kind`, or hides the panel when it is what the panel shows (`diff.toggle`).
pub(crate) fn toggle_kind(state: Store<UiModel>, kind: Kind) {
    if let Some(owner) = active_owner(state) {
        edit(state, |model| model.panels.toggle(&owner.key(), kind));
    }
}
/// Opens a file as a tab, in the panel of the thread it was chosen for (or of the new-thread form).
pub(crate) fn open_file(state: Store<UiModel>, owner_thread: Option<&str>, cwd: &str, path: &str, line: Option<u64>) {
    let Some(environment) = state.destination().peek().as_ref().map(ToString::to_string) else { return };
    let key = key_of(&environment, owner_thread);
    edit(state, |model| model.panels.open_file(&key, cwd, path, line));
}
pub(crate) fn open_pull_request(state: Store<UiModel>, thread_id: &str, target: PullRequestTarget) {
    let Some(environment) = state.destination().peek().as_ref().map(ToString::to_string) else { return };
    let key = key_of(&environment, Some(thread_id));
    edit(state, |model| model.panels.open_pull_request(&key, target));
}
/// Opens a page the conversation holds (an agent's HTML render) as a tab beside the thread.
pub(crate) fn open_attachment(state: Store<UiModel>, thread_id: &str, attachment_id: &str, name: &str) {
    let Some(environment) = state.destination().peek().as_ref().map(ToString::to_string) else { return };
    let key = key_of(&environment, Some(thread_id));
    edit(state, |model| model.panels.open_attachment(&key, attachment_id, name));
}
/// Shows a browser tab: the session's, or the empty one.
pub(crate) fn open_preview(state: Store<UiModel>, environment: &str, thread: &str, tab: Option<&str>) {
    let key = key_of(environment, Some(thread));
    edit(state, |model| model.panels.open_preview(&key, tab));
}
/// Shows the device chosen in the picker.
pub(crate) fn open_device(state: Store<UiModel>, environment: &str, thread: &str, target: t3_client::right_panel::DeviceTarget) {
    let key = key_of(environment, Some(thread));
    edit(state, |model| model.panels.open_device(&key, target));
}
pub(crate) fn open_terminal_in(state: Store<UiModel>, owner: &Owner, terminal_id: &str) {
    edit(state, |model| model.panels.open_terminal(&owner.key(), terminal_id));
}
/// The terminals the open thread's panel holds, so the drawer leaves them alone.
pub(crate) fn terminal_tabs(state: Store<UiModel>, environment: &str, thread: &str) -> Vec<String> {
    peek(state, |model| model.panels.terminal_ids(&key_of(environment, Some(thread))))
}
pub(crate) fn activate(state: Store<UiModel>, owner: &Owner, id: &str) {
    edit(state, |model| model.panels.activate(&owner.key(), id));
}
pub(crate) fn close_surface(state: Store<UiModel>, owner: &Owner, id: &str) {
    let removed = edit(state, |model| {
        let removed = model.panels.close_surface(&owner.key(), id);
        if !model.panels.get(&owner.key()).is_open {
            model.maximized = None;
        }
        removed
    });
    remember(state, owner, removed);
}
fn close_others(state: Store<UiModel>, owner: &Owner, id: &str) {
    let removed = edit(state, |model| model.panels.close_others(&owner.key(), id));
    remember(state, owner, removed);
}
fn close_to_right(state: Store<UiModel>, owner: &Owner, id: &str) {
    let removed = edit(state, |model| model.panels.close_to_right(&owner.key(), id));
    remember(state, owner, removed);
}
fn close_all(state: Store<UiModel>, owner: &Owner) {
    let removed = edit(state, |model| {
        model.maximized = None;
        model.panels.close_all(&owner.key())
    });
    remember(state, owner, removed);
}
/// Closes the shown tab if it is `kind` (the Escape and close buttons of a panel's own header).
pub(crate) fn close_kind(state: Store<UiModel>, kind: Kind) {
    let Some(owner) = active_owner(state) else { return };
    let shown = peek(state, |model| model.panels.active(&owner.key()).cloned());
    if let Some(surface) = shown.filter(|surface| surface.kind() == Some(kind)) {
        close_surface(state, &owner, &surface.id());
    }
}
/// Closes the shown tab, whatever it is (`rightPanel.close`).
pub(crate) fn close_shown(state: Store<UiModel>) {
    let Some(owner) = active_owner(state) else { return };
    if let Some(surface) = peek(state, |model| model.panels.active(&owner.key()).cloned()) {
        close_surface(state, &owner, &surface.id());
    }
}
/// Closes the shown pull request tab.
pub(crate) fn close_shown_pull_request(state: Store<UiModel>) {
    let Some(owner) = active_owner(state) else { return };
    if let Some(surface) = peek(state, |model| model.panels.active(&owner.key()).cloned()).filter(|surface| matches!(surface, Surface::PullRequest(_))) {
        close_surface(state, &owner, &surface.id());
    }
}
/// `rightPanel.toggle`: hides the panel when it is open, else shows it, with a launcher when it holds no tab.
pub(crate) fn toggle_panel(state: Store<UiModel>) {
    let Some(owner) = active_owner(state) else { return };
    edit(state, |model| {
        if model.panels.get(&owner.key()).is_open {
            model.maximized = None;
            model.menu = false;
            model.panels.hide(&owner.key());
        } else {
            model.panels.show(&owner.key());
        }
    });
}
/// `rightPanel.toggleMaximized`: the open panel fills the window, or gives it back.
pub(crate) fn toggle_maximized(state: Store<UiModel>) {
    let Some(owner) = active_owner(state) else { return };
    let key = owner.key();
    edit(state, |model| {
        if !model.panels.get(&key).is_open {
            return;
        }
        model.maximized = if model.maximized.as_deref() == Some(key.as_str()) { None } else { Some(key) };
    });
}
/// `rightPanel.new`: the add menu of the open panel.
pub(crate) fn open_add_menu(state: Store<UiModel>) {
    if is_open(state) {
        edit(state, |model| model.menu = true);
    }
}
/// A thread that was deleted takes its panel with it (`removeThread`).
pub(crate) fn forget_thread(state: Store<UiModel>, thread_id: &str) {
    let Some(environment) = state.destination().peek().as_ref().map(ToString::to_string) else { return };
    let key = key_of(&environment, Some(thread_id));
    edit(state, |model| {
        model.panels.remove_thread(&key);
        if model.maximized.as_deref() == Some(key.as_str()) {
            model.maximized = None;
        }
    });
}

/// `view.reopenClosed`: brings back the tab closed last, in its own thread, which is opened if it is not the one in view. An
/// entry that cannot be restored goes to the back of the history, so the next press tries the one before it.
pub(crate) fn reopen_closed(transport: &TransportHandle, state: Store<UiModel>) {
    let Some(current) = state.destination().peek().as_ref().map(ToString::to_string) else { return };
    let entries = peek(state, |model| model.history.entries.clone());
    let panels = peek(state, |model| model.panels.clone());
    let (threads, projects, live) = {
        let shell = state.shell();
        let shell = shell.peek();
        match shell.snapshot.as_ref() {
            Some(snapshot) => (
                snapshot.threads.iter().map(|thread| (thread.id.to_string(), thread.project_id.to_string())).collect::<Vec<_>>(),
                snapshot.projects.iter().map(|project| project.id.to_string()).collect::<Vec<_>>(),
                true,
            ),
            None => (vec![], vec![], false),
        }
    };
    let catalog = state.environments().peek().records.keys().map(ToString::to_string).collect::<Vec<_>>();
    let (dropped, restore) = {
        let (dropped, restore) = logic::plan_next_reopen(&entries, |entry: &Closed| {
            let here = entry.environment == current;
            logic::Owner {
                environment_known: here || catalog.contains(&entry.environment),
                catalog_ready: true,
                // Another environment's threads are not in this window's shell: skipped, and kept for when it is switched to.
                exists: here && threads.iter().any(|(id, _)| *id == entry.thread_id),
                shell_live: !here || live,
                panel: panels_get(&panels, &entry.key()),
            }
        });
        (dropped.into_iter().map(|entry| entry.id.clone()).collect::<Vec<_>>(), restore.cloned())
    };
    edit(state, |model| {
        for id in &dropped {
            model.history.remove(id);
        }
    });
    let Some(entry) = restore else { return };
    let project_known = threads.iter().find(|(id, _)| *id == entry.thread_id).is_some_and(|(_, project)| projects.contains(project));
    let workspace = matches!(entry.surface, Surface::Files | Surface::File { .. });
    if workspace && !project_known {
        edit(state, |model| model.history.defer(&entry.id));
        return;
    }
    let key = entry.key();
    let browser = entry.browser.clone().filter(|_| entry.environment == current);
    edit(state, |model| {
        match &entry.surface {
            Surface::Diff => model.panels.open(&key, Kind::Diff),
            Surface::Files => model.panels.open(&key, Kind::Files),
            Surface::PullRequests => model.panels.open(&key, Kind::PullRequests),
            Surface::File { cwd, relative_path, reveal_line, .. } => model.panels.open_file(&key, cwd, relative_path, *reveal_line),
            Surface::PullRequest(target) => model.panels.open_pull_request(&key, target.clone()),
            // A browser tab that had a session comes back as a new session, below; one that never had one as the empty tab.
            Surface::Preview { .. } => {
                if browser.is_none() {
                    model.panels.open_preview(&key, None);
                }
            }
            Surface::Device { target: Some(target), title } => {
                model.panels.open_device(&key, target.clone());
                if let Some(title) = title {
                    model.panels.rename_device(&key, &entry.surface.id(), title);
                }
            }
            Surface::Device { target: None, .. } => model.panels.open(&key, Kind::Device),
            Surface::Attachment { attachment_id, name } => model.panels.open_attachment(&key, attachment_id, name),
            // A terminal is never kept.
            Surface::Terminal { .. } => {}
        }
        model.history.remove(&entry.id);
    });
    if let Some(browser) = &browser {
        preview_panel::reopen_session(transport, state, &entry.environment, &entry.thread_id, browser);
    }
    if state.active_thread().peek().as_deref() != Some(entry.thread_id.as_str()) {
        runtime::select_thread(transport, state, entry.thread_id.clone());
    }
}
fn panels_get<'a>(panels: &'a Panels, key: &str) -> &'a ThreadPanel {
    panels.get(key)
}

/// The number of the pull request the open thread's panel shows, for a test to read.
#[cfg(test)]
pub(crate) fn shown_pull_request(state: Store<UiModel>) -> Option<u64> {
    match shown_peek(state) {
        Some(Surface::PullRequest(target)) => Some(target.number),
        _ => None,
    }
}
#[cfg(test)]
pub(crate) fn is_open_peek(state: Store<UiModel>) -> bool {
    active_owner(state).is_some_and(|owner| peek(state, |model| model.panels.get(&owner.key()).is_open))
}
#[cfg(test)]
pub(crate) fn can_reopen_peek(state: Store<UiModel>) -> bool {
    peek(state, |model| !model.history.entries.is_empty())
}
/// The file tabs of the open thread's panel by path, in order, and the file shown, for a test to read.
#[cfg(test)]
pub(crate) fn file_tabs(state: Store<UiModel>) -> Vec<String> {
    let Some(owner) = active_owner(state) else { return vec![] };
    peek(state, |model| model.panels.get(&owner.key()).surfaces.iter().filter_map(|surface| if let Surface::File { relative_path, .. } = surface { Some(relative_path.clone()) } else { None }).collect())
}
#[cfg(test)]
pub(crate) fn shown_file(state: Store<UiModel>) -> Option<(String, Option<u64>)> {
    match shown_peek(state) {
        Some(Surface::File { relative_path, reveal_line, .. }) => Some((relative_path, reveal_line)),
        _ => None,
    }
}

// ---- the choices of the add menu and the launcher -------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    Browser,
    Terminal,
    Files,
    Diff,
    PullRequest,
    PullRequests,
    Device,
}
struct Row {
    choice: Choice,
    label: &'static str,
    shortcut: char,
    available: bool,
    /// The one line the launcher gives for a row that cannot be opened.
    hint: &'static str,
    /// The tooltip of the same row in the add menu.
    reason: &'static str,
}

/// What can be opened now, in the order of the original's menu.
struct Open {
    browser: bool,
    terminal: bool,
    files: bool,
    diff: bool,
    pull_request: bool,
    pull_requests: bool,
    device: bool,
}
const ROWS: usize = 7;
fn rows(open: &Open) -> [Row; ROWS] {
    [
        Row { choice: Choice::Browser, label: "Browser", shortcut: 'B', available: open.browser, hint: "Only available in the desktop app.", reason: "Browser previews are only available in the T3 Code desktop app." },
        Row { choice: Choice::Terminal, label: "Terminal", shortcut: 'T', available: open.terminal, hint: "Available when a project is open.", reason: "Terminal surfaces are only available from a project thread." },
        Row { choice: Choice::Files, label: "Files", shortcut: 'F', available: open.files, hint: "Available when a project is open.", reason: "Files are only available when a project is open." },
        Row { choice: Choice::Diff, label: "Diff", shortcut: 'D', available: open.diff, hint: "Available for Git repositories.", reason: "Diff is only available for server threads in Git repositories." },
        Row { choice: Choice::PullRequest, label: "Pull request", shortcut: 'P', available: open.pull_request, hint: "No pull request on this branch yet.", reason: "This thread's branch has no pull request yet." },
        Row { choice: Choice::PullRequests, label: "Linked pull requests", shortcut: 'L', available: open.pull_requests, hint: "No linked pull requests available.", reason: "No linked pull requests are available for this thread." },
        Row { choice: Choice::Device, label: "Device", shortcut: 'M', available: open.device, hint: "Available from a thread.", reason: "Devices are only available from a thread." },
    ]
}

fn run(state: Store<UiModel>, transport: &TransportHandle, owner: &Owner, choice: Choice) {
    match choice {
        Choice::Browser => preview_panel::add_browser(transport, state, owner),
        Choice::Terminal => {
            let Some(thread) = owner.thread.as_deref() else { return };
            let id = terminal_pane::allocate_id(state, &owner.environment, thread);
            open_terminal_in(state, owner, &id);
        }
        Choice::Files => open_kind_in(state, owner, Kind::Files),
        Choice::Diff => {
            if let Some(thread) = owner.thread.as_deref() {
                // A new tab starts on the branch's changes, as the original's add button does.
                diff_panel::open(state, &owner.environment, thread, t3_client::checkpoints::Scope::Changes, None);
            }
        }
        Choice::PullRequest => {
            if let Some(target) = state.pr_available().peek().clone() {
                pull_request_panel::open(state, target);
            }
        }
        Choice::PullRequests => {
            if let Some(thread) = owner.thread.as_deref() {
                thread_pull_requests_panel::open(state, thread);
            }
        }
        Choice::Device => open_kind_in(state, owner, Kind::Device),
    }
}

/// What can be opened in this panel now, subscribing the caller.
fn availability(state: Store<UiModel>, thread: Option<&str>, cwd: &str, can_read: bool) -> [Row; ROWS] {
    let pull_request = state.pr_available().read().as_ref().is_some_and(|target| Some(target.thread_id.as_str()) == thread);
    let terminals = *state.status().read() == ConnectionStatus::Connected && state.destination().read().as_ref().is_some_and(|id| state.environments().read().allows(id, AuthEnvironmentScope::TerminalOperate));
    rows(&Open {
        browser: thread.is_some() && preview_panel::available(state),
        terminal: thread.is_some() && !cwd.is_empty() && terminals,
        files: !cwd.is_empty(),
        diff: thread.is_some() && can_read,
        pull_request,
        pull_requests: thread.is_some() && thread_pull_requests_panel::supported(state),
        device: thread.is_some(),
    })
}

/// The rows the launcher offers right now, when the window shows one: the panel is open on nothing, and nothing else that takes
/// letters (the add menu, the palette, the model picker) is up. Subscribes the caller.
fn launcher_rows(state: Store<UiModel>) -> Option<Vec<Row>> {
    if *state.view().read() != runtime::View::Chat || state.palette().read().open {
        return None;
    }
    let environment = state.destination().read().as_ref()?.to_string();
    let thread = state.active_thread().read().clone();
    let panels = state.right_panel();
    let model = panels.read();
    let panel = model.panels.get(&key_of(&environment, thread.as_deref()));
    if !panel.is_open || panel.active().is_some() || model.menu {
        return None;
    }
    drop(model);
    // The checkout the Files row opens: the thread's worktree, else its project's root, else the selected project's.
    let shell = state.shell();
    let shell = shell.read();
    let snapshot = shell.snapshot.as_ref()?;
    let selected = state.selected_project().read().clone();
    let project_root = |id: &str| snapshot.projects.iter().find(|project| project.id.as_str() == id).map(|project| project.workspace_root.to_string());
    let cwd = match thread.as_deref().and_then(|id| snapshot.threads.iter().find(|candidate| candidate.id.as_str() == id)) {
        Some(thread) => thread.worktree_path.as_ref().map(ToString::to_string).or_else(|| project_root(thread.project_id.as_str())).unwrap_or_default(),
        None => selected.as_deref().and_then(project_root).unwrap_or_default(),
    };
    let connected = *state.status().read() == ConnectionStatus::Connected;
    let can_read = connected && state.environments().read().allows(&state.destination().read().clone()?, AuthEnvironmentScope::OrchestrationRead);
    Some(availability(state, thread.as_deref(), &cwd, can_read).into_iter().filter(|row| row.available).collect())
}
/// The letters the launcher acts on wherever focus is (`RightPanelEmptyState`'s window listener), for the key claims. Subscribes the caller.
pub(crate) fn launcher_letters(state: Store<UiModel>) -> Vec<char> {
    launcher_rows(state).unwrap_or_default().into_iter().map(|row| row.shortcut).collect()
}
/// Opens the row a pressed letter names, when the launcher shows it. Returns what was opened.
pub(crate) fn run_launcher_letter(state: Store<UiModel>, transport: &TransportHandle, letter: char) -> Option<&'static str> {
    let owner = active_owner(state)?;
    let letter = letter.to_ascii_uppercase();
    let row = launcher_rows(state)?.into_iter().find(|row| row.shortcut == letter)?;
    run(state, transport, &owner, row.choice);
    Some(row.label)
}

fn letter_of(event: &KeyboardEvent) -> Option<char> {
    let modifiers = event.modifiers();
    if modifiers.meta() || modifiers.ctrl() || modifiers.alt() {
        return None;
    }
    match event.key() {
        Key::Character(text) => text.chars().next().map(|letter| letter.to_ascii_uppercase()),
        _ => None,
    }
}

// ---- the components -------------------------------------------------------------------------

/// Where a pull request's last read state is kept: by project, repository and number, as the original's shared summaries are.
pub(crate) fn status_key(project_id: &str, repository: &str, number: u64) -> String {
    format!("{project_id}/{}#{number}", repository.to_lowercase())
}

/// The state glyph of a pull request tab (`PullRequestSurfaceIcon`): the newest link to it among the threads, else what the panel
/// last read of it, else the plain mark. It is its own component so that the shell's changes repaint this glyph and not the bar.
#[component]
fn PullRequestTabIcon(state: Store<UiModel>, target: PullRequestTarget) -> Element {
    let reference = t3_client::pull_request::Reference { project_id: target.project_id.clone(), host: target.host.clone(), repository: target.repository.clone(), number: target.number };
    let status = use_memo(move || {
        let linked = {
            let shell = state.shell();
            let shell = shell.read();
            let host = reference.host.as_deref();
            let links: Vec<serde_json::Value> = shell
                .snapshot
                .iter()
                .flat_map(|snapshot| snapshot.threads.iter())
                .flat_map(|thread| thread.pull_requests.iter().flatten().flatten())
                .filter(|link| host.is_some_and(|host| link.host.as_str().eq_ignore_ascii_case(host)) && link.repository.as_str().eq_ignore_ascii_case(&reference.repository) && link.number.0 == reference.number)
                .filter_map(|link| serde_json::to_value(link).ok())
                .collect();
            t3_client::thread_pull_requests::linked_state(&links, &reference)
        };
        linked.or_else(|| state.pr_states().read().get(&status_key(&reference.project_id, &reference.repository, reference.number)).cloned())
    });
    match status() {
        Some((lifecycle, draft)) => {
            let pr_state = t3_client::thread_pull_requests::State::of(&lifecycle, draft);
            rsx! { span { role: "img", "aria-label": "Pull request state: {pr_state.label()}", crate::pull_request_glyph::Glyph { state: pr_state, stack: false } } }
        }
        None => rsx! { span { class: "pr-tab-unknown", crate::pull_request_glyph::Glyph { state: t3_client::thread_pull_requests::State::Open, stack: false } } },
    }
}

/// What the page says of the tab bar's overflow (`assets/tab_scroll_abi.js`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Overflow {
    has: bool,
    left: bool,
    right: bool,
}

/// The two buttons that scroll a tab bar whose tabs do not fit, and the page script that watches the bar for them (it also brings
/// the shown tab into view when the selection changes). Nothing is drawn while the tabs fit. The measuring is the page's, so a
/// test without a page sees no overflow; the buttons and what they ask of the page are checked in a browser.
#[component]
fn TabScroll(selected: Option<String>) -> Element {
    let mut page = use_signal(|| None::<document::Eval>);
    let mut overflow = use_signal(Overflow::default);
    use_future(move || async move {
        let mut eval = document::eval(include_str!("../assets/tab_scroll_abi.js"));
        page.set(Some(eval));
        while let Ok(message) = eval.recv::<serde_json::Value>().await {
            let next = Overflow { has: message["has"] == true, left: message["left"] == true, right: message["right"] == true };
            if *overflow.peek() != next {
                overflow.set(next);
            }
        }
    });
    use_effect(use_reactive((&selected,), move |_| {
        if let Some(eval) = *page.peek() {
            let _ = eval.send(serde_json::json!({"type":"reveal"}));
        }
    }));
    use_drop(move || {
        if let Some(eval) = *page.peek() {
            let _ = eval.send(serde_json::json!({"type":"stop"}));
        }
    });
    let scroll = move |direction: i32| {
        if let Some(eval) = *page.peek() {
            let _ = eval.send(serde_json::json!({"type":"scroll","direction":direction}));
        }
    };
    let overflow = overflow();
    rsx! {
        if overflow.has {
            div { class: "right-panel-scroll", role: "group", "aria-label": "Scroll panel tabs",
                button { class: "icon-button", r#type: "button", "aria-label": "Scroll tabs left", title: "Scroll tabs left", disabled: !overflow.left, onclick: move |_| scroll(-1), "\u{2039}" }
                button { class: "icon-button", r#type: "button", "aria-label": "Scroll tabs right", title: "Scroll tabs right", disabled: !overflow.right, onclick: move |_| scroll(1), "\u{203a}" }
            }
        }
    }
}

/// The header's button for the whole panel (`rightPanel.toggle`).
#[component]
pub(crate) fn Toggle(state: Store<UiModel>, hint: Option<String>) -> Element {
    let open = is_open(state);
    let title = match (&hint, open) {
        (Some(hint), true) => format!("Hide the panel ({hint})"),
        (Some(hint), false) => format!("Show the panel ({hint})"),
        (None, true) => "Hide the panel".to_owned(),
        (None, false) => "Show the panel".to_owned(),
    };
    rsx! { button { class: if open { "icon-button selected" } else { "icon-button" }, "aria-label": "Toggle panel", "aria-pressed": "{open}", title: "{title}", onclick: move |_| toggle_panel(state), "Panel" } }
}

/// The panel of the open thread, or of the new-thread form: its tab bar and the shown tab. Nothing while it is hidden.
#[component]
pub(crate) fn Shell(state: Store<UiModel>, transport: TransportHandle, environment_key: String, thread_id: Option<String>, cwd: String) -> Element {
    let owner = Owner { environment: environment_key.clone(), thread: thread_id.clone() };
    let key = owner.key();
    let panel = use_memo({
        let key = key.clone();
        move || state.right_panel().read().panels.get(&key).clone()
    });
    let maximized = use_memo({
        let key = key.clone();
        move || state.right_panel().read().maximized.as_deref() == Some(key.as_str())
    });
    let menu = use_memo(move || state.right_panel().read().menu);
    let unsaved = state.file_pending().read().clone();
    let can_read = diff_panel::use_can_read(state);
    let panel = panel();
    if !panel.is_open {
        return rsx! {};
    }
    let rows = availability(state, thread_id.as_deref(), &cwd, can_read());
    let shown = panel.active().cloned();
    rsx! {
        section { class: "right-panel", role: "region", "aria-label": "Panel", "data-maximized": "{maximized()}",
            if let Some(thread_id) = thread_id.clone().filter(|_| preview_panel::available(state)) {
                preview_panel::Sync { state, transport: transport.clone(), environment_key: environment_key.clone(), thread_id }
            }
            div { class: "right-panel-bar",
                div { class: "right-panel-tabs", role: "tablist", "aria-label": "Panel tabs",
                    for (index, surface) in panel.surfaces.iter().enumerate() {
                        {
                            let id = surface.id();
                            let title = match surface {
                                Surface::Preview { resource_id } => preview_panel::title_of(state, &key, resource_id.as_deref()),
                                _ => surface.title(),
                            };
                            let selected = shown.as_ref().is_some_and(|shown| shown.id() == id);
                            let (owner, select_id, close_id, menu_id) = (owner.clone(), id.clone(), id.clone(), id.clone());
                            let (middle_owner, menu_owner) = (owner.clone(), owner.clone());
                            let (middle_id, copy_path) = (id.clone(), if let Surface::File { relative_path, .. } = surface { Some(relative_path.clone()) } else { None });
                            let dirty = matches!(surface, Surface::File { cwd, relative_path, .. } if unsaved.contains(&file_panel::key_of(cwd, relative_path)));
                            let full_title = match surface { Surface::File { relative_path, .. } => relative_path.clone(), _ => title.clone() };
                            rsx! {
                                div { key: "{id}", class: if selected { "right-panel-tab selected" } else { "right-panel-tab" }, "data-surface": "{id}",
                                    oncontextmenu: move |event| {
                                        event.prevent_default();
                                        event.stop_propagation();
                                        let at = event.client_coordinates();
                                        let (owner, id, copy_path) = (menu_owner.clone(), menu_id.clone(), copy_path.clone());
                                        spawn(async move { tab_menu(state, owner, id, index, copy_path, (at.x, at.y)).await });
                                    },
                                    // The middle button closes a tab, as in the original.
                                    onauxclick: move |event| {
                                        if event.trigger_button() == Some(dioxus::html::input_data::MouseButton::Auxiliary) {
                                            event.prevent_default();
                                            event.stop_propagation();
                                            close_surface(state, &middle_owner, &middle_id);
                                        }
                                    },
                                    button { class: "right-panel-tab-name", r#type: "button", role: "tab", "aria-selected": "{selected}", "aria-label": "{title}", title: "{full_title}", onclick: move |_| activate(state, &owner, &select_id), if let Surface::PullRequest(target) = surface { PullRequestTabIcon { state, target: target.clone() } } "{title}" if dirty { span { class: "file-tab-dirty", role: "img", "aria-label": "Unsaved changes", title: "Unsaved changes", " \u{2022}" } } }
                                    button { class: "right-panel-tab-close", r#type: "button", "aria-label": "Close {title} tab", title: "Close {title}", onclick: { let owner = Owner { environment: environment_key.clone(), thread: thread_id.clone() }; move |_| close_surface(state, &owner, &close_id) }, "\u{00d7}" }
                                }
                            }
                        }
                    }
                    div { class: "right-panel-add",
                        button { class: "icon-button", r#type: "button", "aria-label": "Add panel surface", "aria-haspopup": "menu", "aria-expanded": "{menu()}", title: "Add a tab",
                            onclick: move |_| { let next = !menu(); edit(state, |model| model.menu = next); }, "+" }
                        if menu() {
                            AddMenu { state, transport: transport.clone(), owner: owner.clone(), rows: rows_view(&rows) }
                        }
                    }
                }
                TabScroll { selected: shown.as_ref().map(Surface::id) }
                div { class: "right-panel-controls",
                    button { class: "icon-button", r#type: "button", "aria-label": if maximized() { "Restore panel" } else { "Maximize panel" }, "aria-pressed": "{maximized()}", title: if maximized() { "Give the conversation its width back" } else { "Let the panel fill the window" }, onclick: move |_| toggle_maximized(state), if maximized() { "\u{2921}" } else { "\u{2922}" } }
                    button { class: "icon-button", r#type: "button", "aria-label": "Hide panel", title: "Hide the panel; its tabs stay", onclick: move |_| toggle_panel(state), "\u{00d7}" }
                }
            }
            div { class: "right-panel-body",
                match shown {
                    None => rsx! { Launcher { state, transport: transport.clone(), owner: owner.clone(), rows: rows_view(&rows) } },
                    Some(Surface::Diff) => match thread_id.clone() {
                        Some(thread_id) => rsx! { diff_panel::Panel { state, transport: transport.clone(), environment_key: environment_key.clone(), thread_id, cwd: cwd.clone() } },
                        None => rsx! {},
                    },
                    Some(Surface::Files) => rsx! { file_panel::Explorer { key: "{cwd}", state, transport: transport.clone(), cwd: cwd.clone(), owner: thread_id.clone() } },
                    Some(Surface::File { cwd: file_cwd, relative_path, reveal_line, reveal_request_id }) => rsx! {
                        file_panel::Panel { key: "{file_cwd}:{relative_path}:{reveal_request_id}", state, transport: transport.clone(), target: file_panel::Target { owner: thread_id.clone(), cwd: file_cwd, path: relative_path, line: reveal_line, request: reveal_request_id } }
                    },
                    Some(Surface::PullRequest(target)) => {
                        let reference = t3_client::pull_request::Reference { project_id: target.project_id, host: target.host, repository: target.repository, number: target.number };
                        rsx! { pull_request_panel::Panel { key: "{reference.key()}", state, transport: transport.clone(), thread_id: thread_id.clone().unwrap_or_default(), reference } }
                    }
                    Some(Surface::PullRequests) => match thread_id.clone() {
                        Some(thread_id) => rsx! { thread_pull_requests_panel::Panel { state, transport: transport.clone(), thread_id } },
                        None => rsx! {},
                    },
                    Some(Surface::Preview { resource_id }) => match thread_id.clone() {
                        Some(thread_id) => rsx! { preview_panel::Panel { key: "{resource_id:?}", state, transport: transport.clone(), environment_key: environment_key.clone(), thread_id, tab: resource_id } },
                        None => rsx! {},
                    },
                    Some(Surface::Terminal { terminal_id }) => match thread_id.clone() {
                        Some(thread_id) => rsx! { terminal_pane::Tab { key: "{terminal_id}", state, transport: transport.clone(), environment: environment_key.clone(), thread_id, terminal_id, cwd: cwd.clone() } },
                        None => rsx! {},
                    },
                    Some(Surface::Attachment { attachment_id, name }) => rsx! { crate::html_render::PanelTab { key: "{attachment_id}", state, transport: transport.clone(), environment_key: environment_key.clone(), attachment_id, name } },
                    Some(surface @ Surface::Device { .. }) => match thread_id.clone() {
                        Some(thread_id) => rsx! { device_panel::Panel { key: "{surface.id()}", state, transport: transport.clone(), environment_key: environment_key.clone(), thread_id, surface } },
                        None => rsx! {},
                    },
                }
            }
        }
    }
}

/// A row as the menu and the launcher draw it.
#[derive(Clone, PartialEq)]
struct RowView {
    choice: u8,
    label: &'static str,
    shortcut: char,
    available: bool,
    hint: &'static str,
    reason: &'static str,
}
fn rows_view(rows: &[Row; ROWS]) -> Vec<RowView> {
    rows.iter().map(|row| RowView { choice: row.choice as u8, label: row.label, shortcut: row.shortcut, available: row.available, hint: row.hint, reason: row.reason }).collect()
}
fn choice_of(code: u8) -> Choice {
    [Choice::Browser, Choice::Terminal, Choice::Files, Choice::Diff, Choice::PullRequest, Choice::PullRequests, Choice::Device][code as usize]
}

/// The menu of the "+" button: a row opens its tab, and its letter does the same while the menu is open.
#[component]
fn AddMenu(state: Store<UiModel>, transport: TransportHandle, owner: Owner, rows: Vec<RowView>) -> Element {
    let close = move || edit(state, |model| model.menu = false);
    rsx! {
        div { class: "menu-backdrop", onclick: move |_| close() }
        div { class: "context-menu right-panel-menu", role: "menu", "aria-label": "Add a tab", tabindex: "0",
            onmounted: move |event| { drop(event.set_focus(true)); },
            onkeydown: {
                let (rows, owner, transport) = (rows.clone(), owner.clone(), transport.clone());
                move |event| {
                    if event.key() == Key::Escape {
                        event.stop_propagation();
                        close();
                    } else if let Some(letter) = letter_of(&event) {
                        if let Some(row) = rows.iter().find(|row| row.available && row.shortcut == letter) {
                            event.prevent_default();
                            event.stop_propagation();
                            close();
                            run(state, &transport, &owner, choice_of(row.choice));
                        }
                    }
                }
            },
            for row in rows.iter() {
                {
                    let (owner, choice, available, transport) = (owner.clone(), choice_of(row.choice), row.available, transport.clone());
                    rsx! {
                        button { key: "{row.label}", class: "menu-item", role: "menuitem", r#type: "button", "aria-label": "Add {row.label}", "aria-keyshortcuts": "{row.shortcut}", "aria-disabled": "{!available}", disabled: !available, title: if available { "" } else { "{row.reason}" },
                            onclick: move |_| { close(); run(state, &transport, &owner, choice); },
                            span { "{row.label}" } kbd { class: "menu-shortcut", "{row.shortcut}" } }
                    }
                }
            }
        }
    }
}

/// What an empty panel shows: the tabs that can be opened, each with its letter. A row that cannot be opened stays, with a line
/// saying why. Arrows and Enter work while the list is focused.
#[component]
fn Launcher(state: Store<UiModel>, transport: TransportHandle, owner: Owner, rows: Vec<RowView>) -> Element {
    let mut highlight = use_signal(|| -1i32);
    let available: Vec<usize> = rows.iter().enumerate().filter(|(_, row)| row.available).map(|(index, _)| index).collect();
    let current = (!available.is_empty()).then(|| (*highlight.read()).min(available.len() as i32 - 1)).filter(|at| *at >= 0);
    rsx! {
        div { class: "surface-launcher", tabindex: "0", "aria-label": "Open a surface", "data-surface-launcher-keys": "{available.iter().map(|at| rows[*at].shortcut.to_string()).collect::<String>()}",
            onmounted: move |event| { drop(event.set_focus(true)); },
            onkeydown: {
                let (rows, owner, available, transport) = (rows.clone(), owner.clone(), available.clone(), transport.clone());
                move |event| {
                    let modifiers = event.modifiers();
                    if modifiers.meta() || modifiers.ctrl() || modifiers.alt() || available.is_empty() {
                        return;
                    }
                    let count = available.len() as i32;
                    let at = (*highlight.peek()).min(count - 1);
                    match event.key() {
                        Key::ArrowDown | Key::ArrowRight => { event.prevent_default(); highlight.set((at + 1).rem_euclid(count)); }
                        Key::ArrowUp | Key::ArrowLeft => { event.prevent_default(); highlight.set(if at < 0 { count - 1 } else { (at - 1).rem_euclid(count) }); }
                        Key::Enter => {
                            if at >= 0 {
                                event.prevent_default();
                                run(state, &transport, &owner, choice_of(rows[available[at as usize]].choice));
                            }
                        }
                        _ => {
                            if let Some(letter) = letter_of(&event) {
                                if let Some(row) = rows.iter().find(|row| row.available && row.shortcut == letter) {
                                    event.prevent_default();
                                    run(state, &transport, &owner, choice_of(row.choice));
                                }
                            }
                        }
                    }
                }
            },
            div { class: "surface-launcher-list",
                h3 { "Open a surface" }
                for (index, row) in rows.iter().enumerate() {
                    {
                        let (owner, choice, available, transport) = (owner.clone(), choice_of(row.choice), available.clone(), transport.clone());
                        let selected = row.available && current.is_some_and(|at| available[at as usize] == index);
                        rsx! {
                            if row.available {
                                button { key: "{row.label}", class: if selected { "surface-launcher-row selected" } else { "surface-launcher-row" }, r#type: "button", "aria-label": "Open {row.label}",
                                    onmouseenter: move |_| { if let Some(at) = available.iter().position(|candidate| *candidate == index) { highlight.set(at as i32); } },
                                    onmouseleave: move |_| highlight.set(-1),
                                    onclick: move |_| run(state, &transport, &owner, choice),
                                    span { "{row.label}" } kbd { "{row.shortcut}" } }
                            } else {
                                div { key: "{row.label}", class: "surface-launcher-row disabled", tabindex: "0", "aria-disabled": "true", title: "{row.hint}", span { "{row.label}" } span { class: "muted", "{row.hint}" } }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The menu of a tab: copy a file's path, and the four ways to close.
async fn tab_menu(state: Store<UiModel>, owner: Owner, id: String, index: usize, copy_path: Option<String>, at: (f64, f64)) {
    let count = peek(state, |model| model.panels.get(&owner.key()).surfaces.len());
    let Some(choice) = crate::context_menu::show("Tab actions", t3_client::context_menu::tab(copy_path.is_some(), index, count), Some(at)).await else { return };
    match choice.as_str() {
        "copy-path" => {
            if let Some(path) = copy_path {
                crate::sidebar_rows::copy_text(&path);
                notice::push(state, Level::Success, "Path copied", path);
            }
        }
        "close" => close_surface(state, &owner, &id),
        "close-others" => close_others(state, &owner, &id),
        "close-to-right" => close_to_right(state, &owner, &id),
        "close-all" => close_all(state, &owner),
        _ => {}
    }
}
