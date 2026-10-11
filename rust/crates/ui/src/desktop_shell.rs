//! What the desktop app adds around the UI, as `apps/desktop` adds it around `apps/web`: the window and its
//! saved size, the native menu, the server this app starts for itself, the quit shortcut, external links,
//! and the native folder picker. The logic that can be tested without a window is in `t3-desktop`; this puts
//! it on the screen. Nothing here compiles for the web build.
use crate::runtime::{self, UiModel, UiModelStoreExt, View};
use dioxus::{
    desktop::{
        LogicalPosition, LogicalSize, WindowBuilder, muda, use_muda_event_handler, use_wry_event_handler,
        tao::{
            event::{Event, WindowEvent},
            window::Theme,
        },
    },
    prelude::*,
};
use std::{cell::{Cell, RefCell}, path::PathBuf, rc::Rc, sync::Arc, time::Duration};
use t3_client::{connection::ConnectionStatus, themes::Mode};
use t3_desktop::{
    launch::{self, AppDirs, ServerConfig, ServerLauncher},
    menu::{self, Action, Entry, Platform, Role},
    quit_hold::{self, QuitHold},
    supervisor::{Handle, ServerState, Timing},
    url::safe_external_url,
    window_state::{self, Bounds, Debounce, Initial, Observed, Tracker, WindowState},
};

/// The window's title, whatever the page says (`page-title-updated` in the original).
const TITLE: &str = "T3 Code";

/// Where the UI's server comes from.
#[derive(Clone)]
pub(crate) enum Backend {
    /// `T3_SERVER_URL` names a server: this app starts none.
    External,
    /// The server this app started and keeps running.
    Supervised(Handle),
    /// This app would start one but cannot; the person is told why.
    Unavailable(String),
}

/// Handed to the UI at launch.
#[derive(Clone)]
pub(crate) struct Shell {
    dirs: AppDirs,
    backend: Backend,
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

fn start_backend(dirs: &AppDirs) -> Backend {
    if env("T3_SERVER_URL").is_some_and(|url| !url.trim().is_empty()) {
        return Backend::External;
    }
    let home = env("HOME").map(PathBuf::from);
    let result = (|| {
        dirs.check_state_is_not_live_data(home.as_deref())?;
        let binary = launch::find_server_binary(env, std::env::current_exe().ok().as_deref())?;
        let port = launch::find_port(launch::FIRST_PORT, launch::port_is_free).ok_or("No port is free for the server.")?;
        let config = ServerConfig { binary, state_dir: dirs.server_state.clone(), log_file: dirs.server_log.clone(), pid_file: Some(dirs.server_pid.clone()), host: "127.0.0.1".into(), port };
        Handle::spawn(ServerLauncher::new(config), Timing::default(), true).map_err(|error| format!("Cannot start the supervisor: {error}"))
    })();
    match result {
        Ok(handle) => Backend::Supervised(handle),
        Err(message) => Backend::Unavailable(message),
    }
}

/// Opens the window and runs the app. Called from `main` instead of `dioxus::launch`.
pub fn launch(app: fn() -> Element) {
    let dirs = match AppDirs::resolve(env, std::env::consts::OS) {
        Ok(dirs) => dirs,
        Err(message) => {
            eprintln!("T3 Code cannot find a place to keep its files: {message}");
            std::process::exit(1);
        }
    };
    let saved = window_state::load(&dirs.window_state);
    let shell = Shell { backend: start_backend(&dirs), dirs: dirs.clone() };
    let (width, height) = saved.bounds.map_or(window_state::DEFAULT_SIZE, |bounds| (bounds.width, bounds.height));
    let mut window = WindowBuilder::new()
        .with_title(TITLE)
        .with_inner_size(LogicalSize::new(f64::from(width), f64::from(height)))
        .with_min_inner_size(LogicalSize::new(f64::from(window_state::MIN_SIZE.0), f64::from(window_state::MIN_SIZE.1)));
    if let Some(bounds) = saved.bounds {
        window = window.with_position(LogicalPosition::new(f64::from(bounds.x), f64::from(bounds.y)));
    }
    let menu = build_menu(&menu::application_menu(Platform::current(), TITLE), TITLE);
    let config = dioxus::desktop::Config::new()
        .with_window(window)
        .with_menu(menu)
        .with_data_directory(dirs.webview.clone())
        .with_on_window(move |window, _| place_window(&window, &saved));
    dioxus::LaunchBuilder::desktop().with_cfg(config).with_context(shell).launch(app);
}

/// Puts the window where it was, if a display still holds it; otherwise near the top left at the default size.
fn place_window(window: &Arc<dioxus::desktop::tao::window::Window>, saved: &WindowState) {
    let displays: Vec<Bounds> = window
        .available_monitors()
        .map(|monitor| {
            let scale = monitor.scale_factor();
            let (position, size) = (monitor.position().to_logical::<f64>(scale), monitor.size().to_logical::<f64>(scale));
            Bounds { x: position.x as i32, y: position.y as i32, width: size.width as u32, height: size.height as u32 }
        })
        .collect();
    match window_state::initial(saved, &displays) {
        Initial::Saved { bounds, maximized } => {
            // The builder's position is for the content on some platforms; the saved one is the window's own.
            window.set_outer_position(LogicalPosition::new(f64::from(bounds.x), f64::from(bounds.y)));
            window.set_maximized(maximized);
        }
        Initial::Default => {
            window.set_inner_size(LogicalSize::new(f64::from(window_state::DEFAULT_SIZE.0), f64::from(window_state::DEFAULT_SIZE.1)));
            window.set_outer_position(LogicalPosition::new(80.0, 80.0));
        }
    }
}

fn build_menu(entries: &[Entry], app_name: &str) -> muda::Menu {
    let menu = muda::Menu::new();
    for entry in entries {
        if let Entry::Submenu { label, entries } = entry {
            let submenu = muda::Submenu::new(label, true);
            fill(&submenu, entries, app_name);
            let _ = menu.append(&submenu);
            #[cfg(target_os = "macos")]
            match label.as_str() {
                "Window" => submenu.set_as_windows_menu_for_nsapp(),
                "Help" => submenu.set_as_help_menu_for_nsapp(),
                _ => {}
            }
        }
    }
    menu
}

fn fill(submenu: &muda::Submenu, entries: &[Entry], app_name: &str) {
    use muda::PredefinedMenuItem as Predefined;
    for entry in entries {
        let _ = match entry {
            Entry::Separator => submenu.append(&Predefined::separator()),
            Entry::Submenu { label, entries } => {
                let inner = muda::Submenu::new(label, true);
                fill(&inner, entries, app_name);
                submenu.append(&inner)
            }
            Entry::Item { action, label, accelerator } => {
                let accelerator = accelerator.and_then(|text| text.parse::<muda::accelerator::Accelerator>().ok());
                submenu.append(&muda::MenuItem::with_id(action.id(), label, true, accelerator))
            }
            Entry::Role(role) => match role {
                Role::About => submenu.append(&Predefined::about(None, Some(muda::AboutMetadata { name: Some(app_name.to_owned()), version: Some(env!("CARGO_PKG_VERSION").to_owned()), ..Default::default() }))),
                Role::Undo => submenu.append(&Predefined::undo(None)),
                Role::Redo => submenu.append(&Predefined::redo(None)),
                Role::Cut => submenu.append(&Predefined::cut(None)),
                Role::Copy => submenu.append(&Predefined::copy(None)),
                Role::Paste => submenu.append(&Predefined::paste(None)),
                Role::SelectAll => submenu.append(&Predefined::select_all(None)),
                Role::Fullscreen => submenu.append(&Predefined::fullscreen(None)),
                Role::Minimize => submenu.append(&Predefined::minimize(None)),
                Role::Maximize => submenu.append(&Predefined::maximize(None)),
                Role::CloseWindow => submenu.append(&Predefined::close_window(None)),
                Role::Services => submenu.append(&Predefined::services(None)),
                Role::Hide => submenu.append(&Predefined::hide(None)),
                Role::HideOthers => submenu.append(&Predefined::hide_others(None)),
                Role::ShowAll => submenu.append(&Predefined::show_all(None)),
                Role::BringAllToFront => submenu.append(&Predefined::bring_all_to_front(None)),
            },
        };
    }
}

/// What the person sees of the server this app started.
#[derive(Clone, Copy)]
pub(crate) struct ServerView(pub Signal<Option<ServerState>>);

/// The quit hint on screen: the mode it explains, if any.
#[derive(Clone, Copy)]
pub(crate) struct QuitHint(pub Signal<Option<quit_hold::Mode>>);

fn shell() -> Shell {
    consume_context::<Shell>()
}

/// Connects to the server this app started, once it answers, or to `T3_SERVER_URL`. Replaces the web build's
/// connection to the page's own origin.
pub(crate) async fn connect(transport: runtime::TransportHandle, state: Store<UiModel>) {
    let shell = shell();
    match shell.backend {
        Backend::External => runtime::connect(transport, state, runtime::default_address(), String::new()).await,
        Backend::Unavailable(_) => {}
        Backend::Supervised(handle) => {
            let mut view = consume_context::<ServerView>().0;
            let mut changes = handle.state();
            let mut paired = 0;
            loop {
                let current = changes.borrow_and_update().clone();
                view.set(Some(current.clone()));
                // Every start of the server has its own credential. A server that restarted is paired with again,
                // so that a session lost with its state cannot leave the window without one.
                if let ServerState::Ready { address, credential, launch } = current {
                    if launch != paired {
                        paired = launch;
                        let transport = transport.clone();
                        spawn(async move { runtime::pair_and_connect(transport, state, address, credential.0).await });
                    }
                }
                if changes.changed().await.is_err() {
                    return;
                }
            }
        }
    }
}

/// Everything the shell does while the app runs: window state, menu actions, quit shortcut, links, theme.
pub(crate) fn use_shell(state: Store<UiModel>) {
    let shell = use_hook(shell);
    use_context_provider(|| ServerView(Signal::new(match &shell.backend {
        Backend::Unavailable(message) => Some(ServerState::Failed { reason: message.clone() }),
        Backend::Supervised(handle) => Some(handle.state().borrow().clone()),
        Backend::External => None,
    })));
    let hint = use_context_provider(|| QuitHint(Signal::new(None)));
    let persist = use_hook(|| {
        let saved = window_state::load(&shell.dirs.window_state);
        Rc::new(RefCell::new((Tracker::new(&saved), Debounce::default())))
    });
    let quit = use_hook({
        let (shell, persist) = (shell.clone(), persist.clone());
        move || Rc::new(move || quit_app(&shell, &persist))
    });

    // The window's size and place, saved a moment after it stops changing.
    use_wry_event_handler({
        let (shell, persist, quit) = (shell.clone(), persist.clone(), quit.clone());
        move |event, _| {
            let Event::WindowEvent { event, window_id, .. } = event else { return };
            if window_id.clone() != dioxus::desktop::window().id() {
                return;
            }
            match event {
                WindowEvent::Moved(_) | WindowEvent::Resized(_) => {
                    // The event carries the new value; the window itself can still report the old one while the
                    // event is being delivered.
                    let window = dioxus::desktop::window();
                    let scale = window.scale_factor();
                    let (size, position) = match event {
                        WindowEvent::Resized(size) => (Some(size.to_logical::<f64>(scale)), None),
                        WindowEvent::Moved(position) => (None, Some(position.to_logical::<f64>(scale))),
                        _ => (None, None),
                    };
                    let Some(state) = observe(&persist, size, position) else { return };
                    let ticket = persist.borrow_mut().1.note(state);
                    let (path, persist) = (shell.dirs.window_state.clone(), persist.clone());
                    spawn(async move {
                        runtime::delay(window_state::SAVE_DEBOUNCE_MS).await;
                        let due = persist.borrow_mut().1.due(ticket);
                        if let Some(state) = due {
                            let _ = window_state::save(&path, &state);
                        }
                    });
                }
                WindowEvent::CloseRequested => quit(),
                _ => {}
            }
        }
    });

    // The menu.
    let zoom = use_hook(|| Rc::new(Cell::new(1.0f64)));
    use_muda_event_handler({
        let quit = quit.clone();
        move |event| {
            let Some(action) = Action::from_id(&event.id().0) else { return };
            match action {
                Action::OpenSettings => state.view().set(View::General),
                Action::CheckForUpdates => {
                    spawn(async {
                        rfd::AsyncMessageDialog::new()
                            .set_title("Updates unavailable")
                            .set_description("Automatic updates are not available in this build of T3 Code. Install a newer build to update.")
                            .set_buttons(rfd::MessageButtons::Ok)
                            .show()
                            .await;
                    });
                }
                Action::PasteAsText => {
                    let _ = document::eval("(async () => { try { const text = await navigator.clipboard.readText(); document.execCommand('insertText', false, text); } catch (error) { console.warn('Paste as Text:', error); } })();");
                }
                Action::ZoomIn | Action::ZoomOut | Action::ZoomReset => {
                    zoom.set(menu::zoomed(zoom.get(), action));
                    dioxus::desktop::window().set_zoom_level(zoom.get());
                }
                Action::ToggleDevTools => dioxus::desktop::window().devtool(),
                Action::Quit => quit(),
            }
        }
    });

    // The quit shortcut and external links come from the page.
    use_future({
        let (shell, quit) = (shell.clone(), quit.clone());
        move || {
            let (shell, quit) = (shell.clone(), quit.clone());
            async move { page_bridge(state, hint.0, shell, quit).await }
        }
    });

    // The window's chrome follows the appearance setting (`setTheme`).
    if let Some(themes) = try_consume_context::<crate::themes::Themes>() {
        use_effect(move || {
            let theme = match themes.snapshot.read().appearance_mode {
                Mode::System => None,
                Mode::Light => Some(Theme::Light),
                Mode::Dark => Some(Theme::Dark),
            };
            dioxus::desktop::window().set_theme(theme);
        });
    }
}

/// The window as the tracker needs to see it.
fn observe(persist: &Rc<RefCell<(Tracker, Debounce)>>, size: Option<LogicalSize<f64>>, position: Option<LogicalPosition<f64>>) -> Option<WindowState> {
    let window = dioxus::desktop::window();
    let scale = window.scale_factor();
    let size = size.unwrap_or_else(|| window.inner_size().to_logical::<f64>(scale));
    let position = match position {
        Some(position) => position,
        None => window.outer_position().ok()?.to_logical::<f64>(scale),
    };
    let bounds = Bounds { x: position.x.round() as i32, y: position.y.round() as i32, width: size.width.round() as u32, height: size.height.round() as u32 };
    let seen = Observed { bounds, maximized: window.is_maximized(), fullscreen: window.fullscreen().is_some(), minimized: window.is_minimized() };
    persist.borrow_mut().0.observe(seen)
}

/// Stops the server, saves the window, and ends the process.
fn quit_app(shell: &Shell, persist: &Rc<RefCell<(Tracker, Debounce)>>) {
    let pending = persist.borrow_mut().1.flush();
    if let Some(state) = pending {
        let _ = window_state::save(&shell.dirs.window_state, &state);
    }
    if let Backend::Supervised(handle) = &shell.backend {
        // The server's grace period is five seconds; this waits a little longer than that.
        handle.stop_blocking(Duration::from_secs(8));
    }
    std::process::exit(0);
}

/// Runs in the page: Cmd/Ctrl+Q is held back so that quitting can require a hold or a second press (the original
/// intercepts the accelerator before the menu does), and a click on a link is handed to the app, which opens it in
/// the system browser if it is a web page. The page's own click handlers still run first.
const BRIDGE: &str = r#"
{
  if (window.__t3Shell) return;
  window.__t3Shell = true;
  if (window.interpreter) window.interpreter.intercept_link_redirects = false;
  const mac = /Mac/.test(navigator.platform);
  let active = false, timer;
  const touch = () => { active = true; clearTimeout(timer); timer = setTimeout(() => { active = false; }, 3000); };
  const send = (down, event) => dioxus.send({ kind: 'key', down, key: event.key, meta: event.metaKey, control: event.ctrlKey, alt: event.altKey, shift: event.shiftKey, repeat: event.repeat });
  window.addEventListener('keydown', (event) => {
    const modifier = mac ? event.metaKey : event.ctrlKey;
    if (modifier && !event.altKey && !event.shiftKey && event.key.toLowerCase() === 'q') {
      event.preventDefault(); event.stopPropagation(); touch(); send(true, event);
    } else if (active) { send(true, event); }
  }, true);
  window.addEventListener('keyup', (event) => { if (active) send(false, event); }, true);
  document.addEventListener('click', (event) => {
    const link = event.target instanceof Element ? event.target.closest('a[href]') : null;
    if (!link) return;
    event.preventDefault();
    const href = link.getAttribute('href');
    if (href && !href.startsWith('#')) dioxus.send({ kind: 'open', href: link.href });
  }, false);
}
"#;

async fn page_bridge(state: Store<UiModel>, mut hint: Signal<Option<quit_hold::Mode>>, shell: Shell, quit: Rc<dyn Fn()>) {
    let _ = shell;
    let mut eval = document::eval(BRIDGE);
    let hold = Rc::new(RefCell::new(QuitHold::new(cfg!(target_os = "macos"))));
    let timer = Rc::new(Cell::new(0u64));
    let started = std::time::Instant::now();
    loop {
        let message = match eval.recv::<serde_json::Value>().await {
            Ok(message) => message,
            Err(error) => {
                eprintln!("The page bridge ended: {error}");
                return;
            }
        };
        match message["kind"].as_str() {
            Some("open") => {
                if let Some(url) = message["href"].as_str().and_then(safe_external_url) {
                    open_external(&url);
                }
            }
            Some("key") => {
                let key = quit_hold::Key {
                    down: message["down"].as_bool().unwrap_or(false),
                    key: message["key"].as_str().unwrap_or_default().to_owned(),
                    meta: message["meta"].as_bool().unwrap_or(false),
                    control: message["control"].as_bool().unwrap_or(false),
                    alt: message["alt"].as_bool().unwrap_or(false),
                    shift: message["shift"].as_bool().unwrap_or(false),
                    repeat: message["repeat"].as_bool().unwrap_or(false),
                };
                let mode = match state.client_settings().read().confirm_quit {
                    t3_contracts::QuitConfirmationMode::Direct => quit_hold::Mode::Direct,
                    t3_contracts::QuitConfirmationMode::Hold => quit_hold::Mode::Hold,
                    t3_contracts::QuitConfirmationMode::DoubleClick => quit_hold::Mode::DoubleClick,
                };
                let outcome = hold.borrow_mut().key(&key, started.elapsed().as_millis() as u64, mode);
                apply(outcome.effects, &hold, &timer, &mut hint, &quit);
            }
            _ => {}
        }
    }
}

fn apply(effects: Vec<quit_hold::Effect>, hold: &Rc<RefCell<QuitHold>>, timer: &Rc<Cell<u64>>, hint: &mut Signal<Option<quit_hold::Mode>>, quit: &Rc<dyn Fn()>) {
    for effect in effects {
        match effect {
            quit_hold::Effect::Hint(quit_hold::Hint::Down(mode)) => hint.set(Some(mode)),
            quit_hold::Effect::Hint(quit_hold::Hint::Up) => {
                // A released hold's hint stays for the hold time, so a short press still explains itself.
                let shown = *hint.peek();
                if shown == Some(quit_hold::Mode::Hold) {
                    let mut hint = *hint;
                    spawn(async move {
                        runtime::delay(quit_hold::HOLD_MS).await;
                        if *hint.peek() == shown {
                            hint.set(None);
                        }
                    });
                } else {
                    hint.set(None);
                }
            }
            quit_hold::Effect::Quit => quit(),
            quit_hold::Effect::Arm(after) => {
                let ticket = timer.get() + 1;
                timer.set(ticket);
                let (hold, timer, mut hint, quit) = (hold.clone(), timer.clone(), *hint, quit.clone());
                spawn(async move {
                    runtime::delay(after).await;
                    if timer.get() == ticket {
                        let effects = hold.borrow_mut().timer();
                        apply(effects, &hold, &timer, &mut hint, &quit);
                    }
                });
            }
            quit_hold::Effect::Disarm => timer.set(timer.get() + 1),
        }
    }
}

/// Hands a link to the system browser. Only what `safe_external_url` accepted gets here.
pub(crate) fn open_external(url: &str) {
    if let Some(url) = safe_external_url(url) {
        if let Err(error) = webbrowser::open(&url) {
            eprintln!("Cannot open {url}: {error}");
        }
    }
}

/// Whether the connected server runs on this computer, so that a folder picked here means something to it.
pub(crate) fn server_is_local(transport: &runtime::TransportHandle) -> bool {
    runtime::connected_to_loopback(transport)
}

/// The native folder picker (`pickFolder`). None if cancelled.
pub(crate) async fn pick_folder() -> Option<String> {
    let mut dialog = rfd::AsyncFileDialog::new().set_title("Select project folder");
    if let Some(home) = env("HOME") {
        dialog = dialog.set_directory(home);
    }
    dialog.pick_folder().await.map(|folder| folder.path().to_string_lossy().into_owned())
}

/// The strip above the app while the server is not answering.
#[component]
pub(crate) fn Overlays(state: Store<UiModel>) -> Element {
    let Some(view) = try_consume_context::<ServerView>() else { return rsx! {} };
    let hint = use_context::<QuitHint>();
    let connected = *state.status().read() == ConnectionStatus::Connected;
    let backend = shell().backend;
    let banner = match view.0.read().clone() {
        Some(ServerState::Starting { attempt }) if !connected => Some((false, if attempt > 1 { format!("Starting the T3 Code server (attempt {attempt})…") } else { "Starting the T3 Code server…".to_owned() }, None)),
        Some(ServerState::Restarting { in_ms, reason, .. }) => Some((false, format!("The T3 Code server stopped. Starting it again in {} s…", (in_ms as f64 / 1000.0).ceil() as u64), Some(reason))),
        Some(ServerState::Failed { reason }) => Some((true, "The T3 Code server could not be started.".to_owned(), Some(reason))),
        _ => None,
    };
    let shown_hint = *hint.0.read();
    rsx! {
        if let Some((failed, text, detail)) = banner {
            div { class: if failed { "shell-banner failed" } else { "shell-banner" }, role: "status",
                div { class: "shell-banner-text", "{text}",
                    if let Some(detail) = detail { span { class: "shell-banner-detail", "{detail}" } }
                }
                if failed {
                    if let Backend::Supervised(handle) = backend { button { r#type: "button", onclick: move |_| handle.retry(), "Try again" } }
                }
            }
        }
        if let Some(mode) = shown_hint {
            div { class: "quit-hint", role: "status",
                div { class: "quit-hint-pill",
                    {
                        let shortcut = if cfg!(target_os = "macos") { "⌘Q" } else { "Ctrl+Q" };
                        if mode == quit_hold::Mode::Hold { format!("Hold {shortcut} or press twice to quit") } else { format!("Press {shortcut} again to quit") }
                    }
                }
            }
        }
    }
}
