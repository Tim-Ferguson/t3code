//! Keyboard shortcuts and the commands they (and the command palette) run. One listener in the
//! page claims only the chords this module says the app would act on right now; everything else
//! is left to the focused control. The listener's table, the decision and the execution all come
//! from `t3_client::keybindings`, and commands run the same functions the buttons do.
//!
//! Nothing here renders: the table is rebuilt in an effect when the rules, the connection's
//! grants or the few facts a `when` clause can name change, so a key press repaints only what
//! the command it ran changes.
use crate::{
    command_palette, project_create,
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt, View},
    terminal_pane::Catalog,
    themes::Themes,
};
use dioxus::prelude::*;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{cell::RefCell, collections::BTreeSet, rc::Rc};
use t3_client::{
    connection::ConnectionStatus,
    keybindings::{self, Claims, Context, Dom, KeyEvent, Platform, Rule},
    terminal_ui::{Direction, ScopedPanes},
    themes::{Mode, storage::Action},
    thread_nav,
};
use t3_contracts::AuthEnvironmentScope;

/// Whether this build is the desktop app (the window has its own title bar and no browser
/// shortcuts to give way to), as opposed to a page in a browser.
pub(crate) fn desktop() -> bool {
    !cfg!(target_arch = "wasm32")
}
/// What `mod` means here: the browser's `navigator.platform` on the web, the host OS natively.
pub(crate) fn platform() -> Platform {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window().and_then(|window| window.navigator().platform().ok()).map(|name| Platform::parse(&name)).unwrap_or_default()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        Platform::native()
    }
}

/// The rules in force and which commands can run, for whoever shows a shortcut or runs a command.
#[derive(Clone, Copy, PartialEq)]
pub(crate) struct Keys {
    pub rules: Memo<Vec<Rule>>,
    /// Commands the app can carry out now, for the palette's list.
    pub runnable: Memo<BTreeSet<String>>,
    /// The ones a shortcut may fire: the same, except while the palette is open, when only the
    /// commands the palette itself offers keep their shortcuts.
    pub available: Memo<BTreeSet<String>>,
    pub context: Memo<Context>,
    pub platform: Platform,
}
impl Keys {
    /// The shortcut shown beside a command, from the rules of the connected environment.
    pub fn label(&self, command: &str) -> Option<String> {
        keybindings::shortcut_label(&self.rules.read(), command, self.platform, &Context::base(desktop()))
    }
    /// The shortcut of a command that is bound only while `context` holds (the Usage page's letters).
    pub fn label_in(&self, command: &str, context: &str) -> Option<String> {
        keybindings::shortcut_label(&self.rules.read(), command, self.platform, &Context::base(desktop()).with(context, true))
    }
    pub fn can_run(&self, command: &str) -> bool {
        self.runnable.read().contains(command)
    }
}

#[derive(Clone)]
pub(crate) struct Runner {
    pub keys: Keys,
    state: Store<UiModel>,
    transport: TransportHandle,
    catalog: Option<Catalog>,
    themes: Option<Themes>,
    editing: Signal<Option<crate::composer_panels::EditingRun>>,
    picker: crate::model_picker::PickerCtl,
    last_editor: crate::open_in::Last,
}

/// The queued message being edited in the composer, shared so a shortcut can start the edit.
#[derive(Clone, Copy)]
pub(crate) struct QueueEdit(pub Signal<Option<crate::composer_panels::EditingRun>>);

/// The facts of the open thread the commands care about.
struct Open {
    id: String,
    working: bool,
}
fn open_thread(state: Store<UiModel>) -> Option<Open> {
    let id = state.active_thread().read().clone()?;
    let field = state.shell();
    let shell = field.read();
    let thread = shell.snapshot.as_ref()?.threads.iter().find(|thread| thread.id.as_str() == id)?;
    Some(Open { working: thread.status.is_active(), id })
}
/// Thread ids in the order the sidebar lists them: by project, then as the server sent them.
pub(crate) fn sidebar_order(state: Store<UiModel>) -> Vec<String> {
    let field = state.shell();
    let shell = field.read();
    let Some(snapshot) = shell.snapshot.as_ref() else { return vec![] };
    let now = chrono::Utc::now();
    let caps = crate::sidebar_rows::caps(state);
    let shelves = *state.shelves().peek();
    let (working_open, snoozed_open, settled_open) = (shelves.working, shelves.snoozed, shelves.settled);
    // What the sidebar shows: the shelves that are folded away do not take part.
    snapshot.projects.iter().flat_map(|project| {
        let sections = t3_client::sidebar::Sections::of(crate::sidebar_rows::listed(&snapshot.threads, project.id.as_str()), now, caps.support());
        let open = state.active_thread().peek().clone();
        let working = t3_client::sidebar::shelf_rows(&sections.working, working_open, usize::MAX, open.as_deref());
        let snoozed = t3_client::sidebar::shelf_rows(&sections.snoozed, snoozed_open, usize::MAX, open.as_deref());
        let settled = t3_client::sidebar::shelf_rows(&sections.settled, settled_open, t3_client::sidebar::SETTLED_INITIAL, open.as_deref());
        [sections.pinned, sections.active, working, snoozed, settled].concat().into_iter().map(|thread| thread.id.to_string()).collect::<Vec<_>>()
    }).collect()
}
fn allows(state: Store<UiModel>, scope: AuthEnvironmentScope) -> bool {
    *state.status().read() == ConnectionStatus::Connected && state.destination().read().as_ref().is_some_and(|destination| state.environments().read().allows(destination, scope))
}
fn pane_key(state: Store<UiModel>) -> Option<String> {
    let destination = state.destination().read().clone()?;
    let thread = state.active_thread().read().clone()?;
    Some(ScopedPanes::key(destination.as_str(), &thread))
}

pub(crate) fn use_keys(state: Store<UiModel>, transport: TransportHandle) -> Runner {
    let catalog = try_consume_context::<Catalog>();
    let themes = try_consume_context::<Themes>();
    let has_themes = themes.is_some();
    let platform = use_hook(platform);

    // The environment's own rules, with the defaults for commands it does not bind. Until its
    // configuration has arrived, or while another environment's is still held, only the defaults.
    let rules = use_memo(move || {
        let destination = state.destination().read().clone();
        let field = state.typed_config();
        let config = field.read();
        let custom = match (config.as_ref(), destination) {
            (Some(config), Some(destination)) if config.environment.environment_id == destination => keybindings::from_wire(&config.keybindings),
            _ => vec![],
        };
        keybindings::merge_with_defaults(custom)
    });
    let picker = crate::model_picker::PickerCtl { open: use_signal(|| false), jump: use_signal(|| None), step: use_signal(|| None) };
    use_context_provider(|| picker);
    let context = use_memo({
        let catalog = catalog.clone();
        move || {
            let terminal_open = catalog.as_ref().zip(pane_key(state)).is_some_and(|(catalog, key)| catalog.is_open(&key));
            Context::base(desktop())
                .with("terminalOpen", terminal_open)
                .with("diffOpen", state.diff().read().open)
                .with("turnRunning", open_thread(state).is_some_and(|thread| thread.working))
                .with("commandPaletteOpen", state.palette().read().open)
                .with("modelPickerOpen", *picker.open.read())
                .with("usagePageOpen", *state.view().read() == View::Usage)
                .with("draftThreadRoute", *state.view().read() == View::Chat && state.active_thread().read().is_none() && state.selected_project().read().is_some())
        }
    });
    let runnable = use_memo({
        let catalog = catalog.clone();
        move || {
            let mut set: BTreeSet<String> = ["commandPalette.toggle", "theme.select", "appearance.cycle"].map(String::from).into();
            if has_themes {
                set.insert("themeEditor.toggle".into());
            }
            let chat = *state.view().read() == View::Chat;
            let open = open_thread(state);
            let order = sidebar_order(state);
            set.insert("sidebar.toggle".into());
            set.insert("usage.open".into());
            // Both open on any page; with no project they say so, as in the original.
            set.extend(["filePicker.toggle", "projectSearch.toggle"].map(String::from));
            if *state.view().read() == View::Usage {
                set.extend(["usage.cost", "usage.tokens", "usage.limits", "usage.period.day", "usage.period.week", "usage.period.month", "usage.period.quarter"].map(String::from));
            }
            if state.shell().read().snapshot.as_ref().is_some_and(|snapshot| !snapshot.projects.is_empty()) {
                set.insert("chat.new".into());
            }
            if state.shell().read().snapshot.as_ref().is_some_and(|snapshot| !snapshot.projects.is_empty()) {
                set.insert("chat.newLocal".into());
            }
            if allows(state, AuthEnvironmentScope::OrchestrationOperate) {
                if project_create::scratch_available(state) {
                    set.insert("chat.newWithoutProject".into());
                }
                if open.as_ref().is_some_and(|thread| thread.working) {
                    set.insert("thread.stop".into());
                }
            }
            if chat && crate::new_thread::workspace_form_open(state) {
                set.insert("composer.workspace".into());
                if allows(state, AuthEnvironmentScope::OrchestrationOperate) && crate::new_thread::previous_worktree_available(state) {
                    set.insert("composer.previousWorktree".into());
                }
            }
            if state.lifecycle().read().undo.is_some() && allows(state, AuthEnvironmentScope::OrchestrationOperate) {
                set.insert("thread.undo".into());
            }
            if open.is_some() {
                set.insert("thread.copyReference".into());
                if allows(state, AuthEnvironmentScope::OrchestrationOperate) && crate::sidebar_rows::caps(state).pinning {
                    set.insert("thread.pin".into());
                }
                // Only where the server can settle, as in the original.
                if allows(state, AuthEnvironmentScope::OrchestrationOperate) && crate::sidebar_rows::caps(state).settlement {
                    set.insert("thread.settle".into());
                }
            }
            if chat && open.is_none() && state.selected_project().read().is_some() && allows(state, AuthEnvironmentScope::OrchestrationOperate) {
                set.insert("composer.sendBackground".into());
            }
            // The stash belongs to whichever composer is open: a thread's or the new-thread form.
            if chat && (open.is_some() || state.selected_project().read().is_some()) && allows(state, AuthEnvironmentScope::OrchestrationOperate) {
                set.insert("composer.stash".into());
            }
            if chat && open.is_some() {
                if state.diff().read().open || allows(state, AuthEnvironmentScope::OrchestrationRead) {
                    set.insert("diff.toggle".into());
                }
                set.extend(["composer.mode", "composer.effort"].map(String::from));
                if allows(state, AuthEnvironmentScope::OrchestrationOperate) && !crate::open_in::available(state).is_empty() && crate::open_in::thread_cwd(state).is_some() {
                    set.insert("editor.openFavorite".into());
                }
                if allows(state, AuthEnvironmentScope::OrchestrationOperate) {
                    set.insert("modelPicker.toggle".into());
                    if *picker.open.read() {
                        set.extend(["modelPicker.previousProvider", "modelPicker.nextProvider"].map(String::from));
                        set.extend((1..=9).map(|number| format!("modelPicker.jump.{number}")));
                    }
                }
                if allows(state, AuthEnvironmentScope::OrchestrationOperate) {
                    if open.as_ref().is_some_and(|thread| thread.working) {
                        set.insert("composer.sendAlternate".into());
                    }
                    set.insert("composer.sendAndNewThread".into());
                    if state.thread().read().projection.as_ref().is_some_and(|projection| !t3_client::timeline_items::queued_runs(projection).is_empty()) {
                        set.insert("thread.editQueuedMessage".into());
                    }
                    if state.thread().read().projection.as_ref().is_some_and(|projection| !t3_client::timeline_items::queued_runs(projection).is_empty() && t3_client::queue_workflow::can_promote_to_steer(projection)) {
                        set.insert("thread.steerQueuedMessage".into());
                    }
                }
                if let (Some(catalog), Some(key)) = (catalog.as_ref(), pane_key(state)) {
                    if catalog.is_open(&key) || allows(state, AuthEnvironmentScope::TerminalRead) {
                        set.insert("terminal.toggle".into());
                    }
                    // Without operate access a terminal key keeps its native meaning, as it does
                    // when there is nothing to act on. A new terminal or split opens a closed
                    // drawer; closing needs one that is open.
                    if allows(state, AuthEnvironmentScope::TerminalOperate) {
                        set.extend(["terminal.new", "terminal.split", "terminal.splitVertical"].map(String::from));
                        if catalog.is_open(&key) {
                            set.insert("terminal.close".into());
                        }
                    }
                }
                if *state.branch_available().read() {
                    set.insert("composer.branch".into());
                }
                // `pullRequest.copyNumber` belongs to the open pull request panel, as in the original.
                if state.pr_panel().read().is_some() {
                    set.insert("pullRequest.copyNumber".into());
                }
            }
            let current = state.active_thread().read().clone();
            for (command, forward) in [("thread.previous", false), ("thread.next", true)] {
                if thread_nav::adjacent(&order, current.as_deref(), forward).is_some() {
                    set.insert(command.into());
                }
            }
            for number in 1..=order.len().min(9) {
                set.insert(format!("thread.jump.{number}"));
            }
            set
        }
    });
    let available = use_memo(move || {
        // The palette owns the keyboard while it is open; only what it offers itself still works.
        if state.palette().read().open {
            runnable.read().iter().filter(|command| matches!(command.as_str(), "commandPalette.toggle" | "theme.select" | "appearance.cycle" | "filePicker.toggle" | "projectSearch.toggle")).cloned().collect()
        } else {
            runnable.read().clone()
        }
    });
    let keys = Keys { rules, runnable, available, context, platform };
    let editing = use_signal(|| None::<crate::composer_panels::EditingRun>);
    use_context_provider(|| QueueEdit(editing));
    let last_editor = crate::open_in::use_last();
    use_context_provider(|| last_editor);
    let runner = Runner { keys, state, transport, catalog, themes, editing, picker, last_editor };
    use_context_provider(|| keys);
    use_context_provider(|| runner.clone());

    // Keys reach the runner through one channel, whether the page's listener or a test sends them.
    let (sender, receiver) = use_hook(|| {
        let (sender, receiver) = futures_channel::mpsc::unbounded::<(KeyEvent, Dom)>();
        (sender, Rc::new(RefCell::new(Some(receiver))))
    });
    #[cfg(test)]
    INJECT.with(|slot| *slot.borrow_mut() = Some(sender.clone()));
    use_future({
        let runner = runner.clone();
        move || {
            let mut receiver = receiver.borrow_mut().take().expect("the key runner mounts once");
            let runner = runner.clone();
            async move {
                while let Some((event, dom)) = receiver.next().await {
                    runner.handle(&event, dom);
                }
            }
        }
    });

    // The page's listener: told which chords to claim, and reporting the ones it did.
    let mut page = use_signal(|| None::<document::Eval>);
    use_future(move || {
        let sender = sender.clone();
        async move {
            let script = format!("const args={};\n{}", json!({"mac":platform.is_mac()}), include_str!("../assets/keys_abi.js"));
            let mut eval = document::eval(&script);
            page.set(Some(eval));
            while let Ok(message) = eval.recv::<Value>().await {
                if let Some(event) = KeyEvent::from_json(&message) {
                    let _ = sender.unbounded_send((event, Dom::from_json(&message["dom"])));
                }
            }
        }
    });
    let claims = use_memo(move || Claims::build(&rules.read(), platform, &context.read(), |command| available.read().contains(command)));
    use_effect(move || {
        let table = claims.read().to_json();
        if let Some(eval) = *page.read() {
            let _ = eval.send(json!({"type":"claims","table":table}));
        }
    });
    use_drop(move || {
        if let Some(eval) = *page.peek() {
            let _ = eval.send(json!({"type":"stop"}));
        }
    });

    // A keybindings file that stops parsing is worth saying so when it happens (not when the app
    // first loads it, where the Keybindings page shows it).
    let mut seen_issues = use_signal(|| None::<(String, Vec<String>)>);
    use_effect(move || {
        let destination = state.destination().read().as_ref().map(ToString::to_string).unwrap_or_default();
        let issues: Option<Vec<String>> = state.typed_config().read().as_ref().map(|config| {
            config
                .issues
                .0
                .iter()
                .map(|issue| match issue {
                    t3_contracts::ServerConfigIssue::MalformedKeybindings { message } | t3_contracts::ServerConfigIssue::InvalidKeybinding { message, .. } => message.to_string(),
                })
                .collect()
        });
        let Some(issues) = issues else { return };
        let previous = seen_issues.peek().clone();
        if previous.as_ref().map(|(_, issues)| issues) == Some(&issues) {
            return;
        }
        seen_issues.set(Some((destination.clone(), issues.clone())));
        if let (Some((before, _)), Some(first)) = (previous, issues.first()) {
            if before == destination {
                crate::notice::push(state, crate::notice::Level::Warning, "Invalid keybindings configuration", first.clone());
            }
        }
    });
    runner
}

#[cfg(test)]
thread_local! {
    pub(crate) static INJECT: RefCell<Option<futures_channel::mpsc::UnboundedSender<(KeyEvent, Dom)>>> = const { RefCell::new(None) };
}
#[cfg(test)]
thread_local! {
    /// The command each pressed key ran, or None for a key the app left alone, in order.
    pub(crate) static HANDLED: RefCell<Vec<Option<String>>> = const { RefCell::new(Vec::new()) };
}
/// Sends a key to the mounted app the way the page's listener does.
#[cfg(test)]
pub(crate) fn press(event: KeyEvent, dom: Dom) {
    INJECT.with(|slot| slot.borrow().as_ref().expect("the app is mounted").unbounded_send((event, dom)).expect("the key runner is running"));
}

impl Runner {
    /// A key the page claimed, or that a test pressed. Returns the command it ran.
    pub fn handle(&self, event: &KeyEvent, dom: Dom) -> Option<String> {
        let ran = self.decide_and_run(event, dom);
        #[cfg(test)]
        HANDLED.with(|handled| handled.borrow_mut().push(ran.clone()));
        ran
    }
    fn decide_and_run(&self, event: &KeyEvent, dom: Dom) -> Option<String> {
        let command = {
            let context = self.keys.context.peek().clone().with_dom(dom);
            let rules = self.keys.rules.peek();
            let available = self.keys.available.peek();
            keybindings::decide(event, &rules, self.keys.platform, &context, |command| available.contains(command))?.command.clone()
        };
        // A held key repeats; the first press already ran the command.
        if !event.repeat {
            self.execute(&command);
        }
        Some(command)
    }

    /// Runs a command: what its shortcut does, and what the palette's entry for it does.
    pub fn execute(&self, command: &str) {
        let state = self.state;
        match command {
            "sidebar.toggle" => {
                let open = state.peek().sidebar_open;
                state.sidebar_open().set(!open);
            }
            "commandPalette.toggle" => command_palette::toggle(state),
            "filePicker.toggle" => command_palette::toggle_mode(state, command_palette::Intent::Files),
            "projectSearch.toggle" => command_palette::toggle_mode(state, command_palette::Intent::Content),
            "theme.select" => command_palette::open(state, command_palette::Intent::Theme),
            "appearance.cycle" => {
                if let Some(themes) = &self.themes {
                    let next = match themes.snapshot.peek().appearance_mode {
                        Mode::System => Mode::Light,
                        Mode::Light => Mode::Dark,
                        Mode::Dark => Mode::System,
                    };
                    themes.change(Action::Mode(next));
                }
            }
            // Both start a thread in the project in use. The original's `chat.new` asks which
            // project first when there are several; this app's does not yet.
            "chat.new" | "chat.newLocal" => runtime::new_thread(&self.transport, state, None),
            "usage.open" => crate::usage_page::open(state),
            "usage.cost" => crate::usage_page::select_metric(&self.transport, state, "cost"),
            "usage.tokens" => crate::usage_page::select_metric(&self.transport, state, "tokens"),
            "usage.limits" => crate::usage_page::select_metric(&self.transport, state, "limits"),
            "usage.period.day" => crate::usage_page::select_period(&self.transport, state, 1),
            "usage.period.week" => crate::usage_page::select_period(&self.transport, state, 7),
            "usage.period.month" => crate::usage_page::select_period(&self.transport, state, 30),
            "usage.period.quarter" => crate::usage_page::select_period(&self.transport, state, 90),
            "chat.newWithoutProject" => project_create::start_scratch(&self.transport, state),
            "diff.toggle" => crate::diff_panel::toggle(state),
            "editor.openFavorite" => {
                let last = self.last_editor.0.peek().clone();
                if let (Some(editor), Some(cwd)) = (crate::open_in::favorite(state, last.as_deref()), crate::open_in::thread_cwd(state)) {
                    crate::open_in::open(&self.transport, state, Signal::new(false), self.last_editor.0, cwd, editor, false);
                }
            }
            "terminal.toggle" => {
                if let (Some(catalog), Some(key)) = (&self.catalog, pane_key(state)) {
                    catalog.toggle(&key);
                }
            }
            "terminal.new" | "terminal.split" | "terminal.splitVertical" => {
                if let (Some(catalog), Some(key)) = (&self.catalog, pane_key(state)) {
                    let split = match command {
                        "terminal.split" => Some(Direction::Horizontal),
                        "terminal.splitVertical" => Some(Direction::Vertical),
                        _ => None,
                    };
                    catalog.add_terminal(&key, split);
                }
            }
            "terminal.close" => {
                if let (Some(catalog), Some(key)) = (&self.catalog, pane_key(state)) {
                    catalog.request_close(&key, None);
                }
            }
            "themeEditor.toggle" => {
                if let Some(themes) = &self.themes {
                    crate::theme_library::toggle_editor(themes);
                }
            }
            "pullRequest.copyNumber" => {
                let open = state.pr_panel().peek().clone();
                if let Some(target) = open {
                    let text = format!("#{}", target.reference.number);
                    let args = serde_json::json!(text);
                    dioxus::document::eval(&format!("navigator.clipboard?.writeText({args});"));
                    crate::notice::push(state, crate::notice::Level::Success, "PR number copied", text);
                }
            }
            "composer.workspace" => crate::new_thread::focus_workspace(),
            "composer.previousWorktree" => crate::new_thread::use_previous_worktree(state),
            "thread.undo" => crate::thread_lifecycle::undo(state, &self.transport),
            "composer.sendBackground" => {
                let sends = *state.background_sends().peek();
                state.background_sends().set(sends + 1);
            }
            "composer.branch" => {
                let toggles = *state.branch_toggles().peek();
                state.branch_toggles().set(toggles + 1);
            }
            "modelPicker.toggle" => {
                let mut open = self.picker.open;
                let now = *open.peek();
                open.set(!now);
            }
            "modelPicker.previousProvider" | "modelPicker.nextProvider" => {
                let mut step = self.picker.step;
                step.set(Some(if command == "modelPicker.nextProvider" { 1 } else { -1 }));
            }
            jump if jump.starts_with("modelPicker.jump.") => {
                if let Some(number) = jump.rsplit('.').next().and_then(|number| number.parse::<usize>().ok()) {
                    let mut target = self.picker.jump;
                    target.set(Some(number));
                }
            }
            "composer.stash" => crate::prompt_stash::shortcut(&self.transport, state),
            "composer.mode" => {
                let _ = document::eval("document.querySelector('.thread-controls select[aria-label=\"Permissions\"]')?.focus();");
            }
            "composer.effort" => {
                // The first trait of the model is its effort; the list opens where the page can open one.
                let _ = document::eval("const select=document.querySelector('.thread-controls .model-traits select');if(select){select.focus();try{select.showPicker?.();}catch{}}");
            }
            "composer.sendAlternate" => {
                let queue = state.client_settings().peek().follow_up_behavior == t3_contracts::FollowUpBehavior::Queue;
                runtime::send_with(&self.transport, state, runtime::SendOptions { mode: Some(t3_client::timeline_items::dispatch_mode(true, true, queue)), ..Default::default() });
            }
            // Sends what is in the box and opens a fresh composer in the same project, so the thread
            // goes on without waiting (`composer.sendAndNewThread`). An empty box does neither.
            "composer.sendAndNewThread" => {
                if !state.draft().peek().trim().is_empty() {
                    runtime::send_with(&self.transport, state, runtime::SendOptions::default());
                    runtime::new_thread(&self.transport, state, None);
                }
            }
            "thread.editQueuedMessage" => {
                let latest = state.peek().thread.projection.as_ref().and_then(|projection| t3_client::timeline_items::queued_runs(projection).pop());
                if let Some(run) = latest {
                    if let Some(thread) = open_thread(state) {
                        crate::composer_panels::begin_edit(state, self.editing, thread.id, run.run_id, run.text);
                    }
                }
            }
            // Sends the first queued message into the running turn, as the Steer button on it does.
            "thread.steerQueuedMessage" => {
                let first = state.peek().thread.projection.as_ref().and_then(|projection| t3_client::timeline_items::queued_runs(projection).into_iter().next());
                if let (Some(run), Some(thread)) = (first, open_thread(state)) {
                    crate::composer_panels::steer(&self.transport, state, &thread.id, &run.run_id);
                }
            }
            "thread.stop" => {
                if let Some(thread) = open_thread(state) {
                    runtime::stop_thread(&self.transport, state, &thread.id);
                }
            }
            "thread.pin" => crate::sidebar_rows::toggle_pin(&self.transport, state),
            "thread.settle" => crate::sidebar_rows::toggle_settle(&self.transport, state),
            "thread.copyReference" => crate::sidebar_rows::copy_reference(state),
            "thread.previous" | "thread.next" => {
                let order = sidebar_order(state);
                let current = state.active_thread().peek().clone();
                if let Some(id) = thread_nav::adjacent(&order, current.as_deref(), command == "thread.next") {
                    runtime::select_thread(&self.transport, state, id.to_owned());
                }
            }
            jump => {
                if let Some(index) = thread_nav::jump_index(jump) {
                    if let Some(id) = sidebar_order(state).get(index) {
                        runtime::select_thread(&self.transport, state, id.clone());
                    }
                }
            }
        }
    }
}

/// Puts the cursor in the message box of the open thread or of the new-thread form.
pub(crate) fn focus_composer() {
    let _ = document::eval("document.querySelector('.composer textarea, .new-thread-prompt textarea')?.focus();");
}
