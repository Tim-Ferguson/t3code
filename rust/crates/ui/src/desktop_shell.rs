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
    /// SSH environments (`DesktopSshEnvironment`); none when the runtime for it cannot be made.
    ssh: Option<t3_desktop::ssh::Ssh>,
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
        // Asked now, beside the window opening, so the first start of the server does not wait for the person's shell.
        let login_shell = std::sync::Arc::new(t3_desktop::shell_env::Cached::new(t3_desktop::shell_env::Config::for_process()));
        std::thread::Builder::new().name("t3-login-shell".into()).spawn({
            let login_shell = login_shell.clone();
            move || {
                login_shell.get();
            }
        }).map_err(|error| format!("Cannot ask the login shell: {error}"))?;
        let config = ServerConfig { binary, state_dir: dirs.server_state.clone(), log_file: dirs.server_log.clone(), pid_file: Some(dirs.server_pid.clone()), host: "127.0.0.1".into(), port, login_shell: Some(login_shell) };
        let answers = crate::desktop_updates::answers(&dirs.root.join(t3_desktop::updates::SETTINGS_FILE));
        Handle::spawn(ServerLauncher::new(config).with_control_channel(answers), Timing::default(), true).map_err(|error| format!("Cannot start the supervisor: {error}"))
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
    let ssh = match t3_desktop::ssh::Ssh::new(t3_desktop::ssh::Config::from_env(env, env!("CARGO_PKG_VERSION"))) {
        Ok(ssh) => Some(ssh),
        Err(error) => {
            eprintln!("SSH environments are unavailable: {error}");
            None
        }
    };
    let shell = Shell { backend: start_backend(&dirs), dirs: dirs.clone(), ssh };
    let (width, height) = saved.bounds.map_or(window_state::DEFAULT_SIZE, |bounds| (bounds.width, bounds.height));
    let mut window = WindowBuilder::new()
        .with_title(TITLE)
        .with_inner_size(LogicalSize::new(f64::from(width), f64::from(height)))
        .with_min_inner_size(LogicalSize::new(f64::from(window_state::MIN_SIZE.0), f64::from(window_state::MIN_SIZE.1)));
    if let Some(bounds) = saved.bounds {
        window = window.with_position(LogicalPosition::new(f64::from(bounds.x), f64::from(bounds.y)));
    }
    // macOS: the page draws under a transparent title bar, with the traffic lights over its top left corner (`hiddenInset`).
    #[cfg(target_os = "macos")]
    {
        use dioxus::desktop::tao::platform::macos::WindowBuilderExtMacOS;
        window = window.with_titlebar_transparent(true).with_fullsize_content_view(true).with_title_hidden(true);
    }
    let menu = build_menu(&menu::application_menu(Platform::current(), TITLE), TITLE);
    let config = dioxus::desktop::Config::new()
        .with_close_behaviour(window_close_behaviour())
        .with_window(window)
        .with_menu(menu)
        .with_data_directory(dirs.webview.clone())
        .with_on_window(move |window, _| place_window(&window, &saved));
    dioxus::LaunchBuilder::desktop().with_cfg(config).with_context(shell).launch(app);
}

/// What closing the window does: on macOS it hides (the app and its server keep running), elsewhere it closes and the app ends.
fn window_close_behaviour() -> dioxus::desktop::WindowCloseBehaviour {
    if cfg!(target_os = "macos") { dioxus::desktop::WindowCloseBehaviour::WindowHides } else { dioxus::desktop::WindowCloseBehaviour::WindowCloses }
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

/// Where the saved connections are kept: an encrypted file in the app's data directory, its key in the macOS keychain. A profile
/// given a data directory of its own (`T3_UI_DATA_DIR`) keeps the key in that directory instead, so that it never reaches the
/// person's keychain. A platform with no secret service wired up saves nothing (`t3_desktop::catalog_vault`).
pub(crate) fn catalog_backend() -> Option<Rc<dyn crate::connection_store::Backend>> {
    let shell = try_consume_context::<Shell>()?;
    let own_directory = env("T3_UI_DATA_DIR").is_some_and(|path| !path.trim().is_empty());
    let keys = t3_desktop::catalog_vault::key_store(&shell.dirs.root, own_directory)?;
    Some(Rc::new(crate::connection_store::Encrypted(t3_desktop::catalog_vault::Vault::new(shell.dirs.root.join("connection-catalog.json"), keys))))
}

/// Where the `t3` command goes and what it runs, for the settings row: the app's bundled server program, linked from the app's own
/// folder into a folder on PATH. `T3_CLI_COMMAND_DIRS` (colon separated) names the folders a link may go in instead of the person's
/// own, which is how it is tried without touching a real PATH folder. None where the app has no command to offer.
pub(crate) fn cli_environment() -> Option<t3_desktop::cli_command::Environment> {
    use t3_desktop::cli_command::Environment;
    let shell = try_consume_context::<Shell>()?;
    let executable = launch::find_server_binary(env, std::env::current_exe().ok().as_deref()).ok()?;
    let home = PathBuf::from(env("HOME")?);
    let named: Vec<PathBuf> = env("T3_CLI_COMMAND_DIRS").map(|dirs| dirs.split(':').filter(|dir| !dir.is_empty()).map(PathBuf::from).collect()).unwrap_or_default();
    let mut path = named.clone();
    path.extend(std::env::var_os("PATH").map(|value| std::env::split_paths(&value).collect::<Vec<_>>()).unwrap_or_default());
    let candidates = if named.is_empty() { Environment::candidates(&home, cfg!(target_os = "macos")) } else { named };
    Some(Environment { launcher_dir: shell.dirs.root.join("bin"), executable, state_dir: shell.dirs.server_state.clone(), candidates, path, packaged: true })
}

/// The file the app keeps its own settings in (the update track).
pub(crate) fn settings_path() -> Option<PathBuf> {
    try_consume_context::<Shell>().map(|shell| shell.dirs.root.join(t3_desktop::updates::SETTINGS_FILE))
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

    // SSH environments: the Connections page's gateway, and the window coming forward when ssh asks for a password.
    if let Some(ssh) = shell.ssh.clone() {
        let backend = shell.backend.clone();
        let saved = shell.dirs.root.join("ssh-environments.json");
        use_context_provider(|| {
            let ssh = ssh.clone();
            crate::ssh_environments::GatewayHandle(Rc::new(crate::ssh_gateway::DesktopGateway { ssh, saved, primary: Box::new(move || primary_server(&backend)) }))
        });
        use_future(move || {
            let mut feed = ssh.prompts().watch();
            async move {
                let mut open = 0;
                loop {
                    let now = feed.borrow_and_update().len();
                    if now > open {
                        let window = dioxus::desktop::window();
                        window.set_minimized(false);
                        window.set_visible(true);
                        window.set_focus();
                    }
                    open = now;
                    if feed.changed().await.is_err() {
                        return;
                    }
                }
            }
        });
    }

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
                // The original quits when its last window closes, except on macOS, where the app stays in the dock with its server
                // running and a click on the dock icon opens the window again. The window is only hidden there (`window_close_behaviour`).
                WindowEvent::CloseRequested if !cfg!(target_os = "macos") => quit(),
                _ => {}
            }
        }
    });
    // Dock icon clicked with no window showing.
    use_wry_event_handler(|event, _| {
        if let Event::Reopen { has_visible_windows: false, .. } = event {
            let window = dioxus::desktop::window();
            window.set_minimized(false);
            window.set_visible(true);
            window.set_focus();
        }
    });

    // The menu.
    let zoom = use_hook(|| Rc::new(Cell::new(1.0f64)));
    use_muda_event_handler({
        let quit = quit.clone();
        move |event| {
            if let Some(choice) = event.id().0.strip_prefix(CONTEXT_ID) {
                if let Some(act) = MENU_CHOICE.with(|pending| pending.borrow_mut().take()) {
                    act(choice);
                }
                return;
            }
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
                    ZOOM.with(|shown| shown.set(zoom.get()));
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
    // The tunnels close, and the servers this app started on other machines stop, before the app ends.
    if let Some(ssh) = &shell.ssh {
        ssh.shutdown_blocking(Duration::from_secs(10));
    }
    if let Backend::Supervised(handle) = &shell.backend {
        // The server's grace period is five seconds; this waits a little longer than that. The file naming its process id
        // goes with it; one that outlived the wait stays, as the only way to find the server that is still running.
        if handle.stop_blocking(Duration::from_secs(8)) {
            let _ = std::fs::remove_file(&shell.dirs.server_pid);
        }
    }
    std::process::exit(0);
}

/// The address of the server this app started, once it answers; the page's own for a server the person named.
/// None while the server starts, restarts or has failed.
pub(crate) fn server_address() -> Option<String> {
    match try_consume_context::<ServerView>()?.0.read().as_ref() {
        Some(ServerState::Ready { address, .. }) => Some(address.clone()),
        Some(_) => None,
        None => Some(runtime::default_address()),
    }
}

/// The endpoints the original's desktop (`DesktopServerExposure`) offers for pairing a device, as this app has them: the server it
/// started listens on loopback only, so "This machine" plus the HTTPS addresses the person set in `T3CODE_DESKTOP_HTTPS_ENDPOINTS`
/// (`t3_desktop::network::app_endpoints`; the local network and Tailscale ones need a wider bind or Serve, which this app does not
/// start). A server the person named is not this app's to describe.
pub(crate) fn advertised_endpoints() -> crate::access_page::AdvertisedEndpoints {
    let Some(address) = server_address().filter(|_| matches!(shell().backend, Backend::Supervised(_))) else { return Default::default() };
    let Some(port) = address.rsplit(':').next().unwrap_or_default().trim_end_matches('/').parse::<u16>().ok() else { return Default::default() };
    crate::access_page::AdvertisedEndpoints { endpoints: t3_desktop::network::app_endpoints(port, &env("T3CODE_DESKTOP_HTTPS_ENDPOINTS").unwrap_or_default()), default_key: None }
}

/// How the window reaches the server this app started, now: its address with the credential of this start, or the one the person named.
fn primary_server(backend: &Backend) -> Option<crate::ssh_environments::Primary> {
    use crate::ssh_environments::Primary;
    match backend {
        Backend::External => Some(Primary::Open { address: runtime::default_address() }),
        Backend::Unavailable(_) => None,
        Backend::Supervised(handle) => match handle.state().borrow().clone() {
            ServerState::Ready { address, credential, .. } => Some(Primary::Pair { address, credential: credential.0 }),
            _ => None,
        },
    }
}

/// Starts the server this app runs again (the failed-import screen's "Try again": the server that stopped importing keeps
/// serving the failure until it is started anew). A server the person named is not this app's to restart.
pub(crate) fn restart_server() {
    if let Backend::Supervised(handle) = shell().backend {
        handle.restart();
    }
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
  // The title bar is the page's own (the window has no native one on macOS): a press on the header's empty space moves the window.
  document.addEventListener('mousedown', (event) => {
    if (event.button !== 0 || !(event.target instanceof Element)) return;
    if (!event.target.closest('.brand, .workspace-header') || event.target.closest('button, a, input, select, textarea, summary, [role=button], [role=menu], [contenteditable]')) return;
    dioxus.send({ kind: 'drag' });
  }, false);
  // The window's own right-click menu (`installContextMenu` in the original): cut, copy, paste and select all, "Copy Link" over a link and
  // "Copy Image" over an image, in place of the web view's. Menus of the app's own (the sidebar's rows, the open menus) and the terminal keep theirs.
  document.addEventListener('contextmenu', (event) => {
    const target = event.target;
    if (!(target instanceof Element) || target.closest('.sidebar, .menu-backdrop, .context-menu, [role=menu], .notices, canvas, [class*=terminal]')) return;
    event.preventDefault();
    const link = target.closest('a[href]');
    const image = target instanceof HTMLImageElement ? target : target.closest('img');
    dioxus.send({ kind: 'context', link: link ? link.href : null, image: image ? image.currentSrc || image.src : null });
  }, false);
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
            Some("context") => {
                page_context_menu(message["link"].as_str().and_then(safe_external_url), message["image"].as_str().map(str::to_owned));
            }
            Some("drag") => {
                let _ = dioxus::desktop::window().drag_window();
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

thread_local! {
    /// What the open native context menu does with the entry chosen, until a choice or the next menu replaces it.
    static MENU_CHOICE: RefCell<Option<Box<dyn FnOnce(&str)>>> = const { RefCell::new(None) };
}

/// The id prefix of the entries of a native context menu, apart from the application menu's.
const CONTEXT_ID: &str = "ctx:";

/// Shows the window's own context menu where the pointer is. Cut, copy, paste and select all act on the web view as the
/// system's own editing menu does; "Copy Link" and "Copy Image" are the original's other two entries.
fn page_context_menu(link: Option<String>, image: Option<String>) {
    #[cfg(target_os = "macos")]
    {
        use dioxus::desktop::{muda::ContextMenu, tao::platform::macos::WindowExtMacOS};
        use muda::PredefinedMenuItem as Predefined;
        let menu = muda::Menu::new();
        if let Some(link) = &link {
            let _ = menu.append(&muda::MenuItem::with_id(format!("{CONTEXT_ID}copy-link"), "Copy Link", true, None));
            let _ = menu.append(&Predefined::separator());
            let _ = link;
        }
        if image.is_some() {
            let _ = menu.append(&muda::MenuItem::with_id(format!("{CONTEXT_ID}copy-image"), "Copy Image", true, None));
            let _ = menu.append(&Predefined::separator());
        }
        let _ = menu.append(&Predefined::cut(None));
        let _ = menu.append(&Predefined::copy(None));
        let _ = menu.append(&Predefined::paste(None));
        let _ = menu.append(&Predefined::select_all(None));
        MENU_CHOICE.with(|choice| {
            *choice.borrow_mut() = Some(Box::new(move |id| match id {
                "copy-link" => {
                    if let Some(link) = &link {
                        let text = serde_json::to_string(link).unwrap_or_default();
                        let _ = document::eval(&format!("navigator.clipboard.writeText({text})"));
                    }
                }
                "copy-image" => {
                    if let Some(image) = &image {
                        let url = serde_json::to_string(image).unwrap_or_default();
                        let _ = document::eval(&format!("(async()=>{{try{{const blob=await (await fetch({url})).blob();await navigator.clipboard.write([new ClipboardItem({{[blob.type||'image/png']:blob}})]);}}catch(error){{console.warn('Copy Image:',error);}}}})()"));
                    }
                }
                _ => {}
            }));
        });
        let view = dioxus::desktop::window().ns_view();
        // Returns when the menu closes; a choice arrives through the menu event handler.
        unsafe { menu.show_context_menu_for_nsview(view, None) };
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (link, image);
}

thread_local! {
    /// The page's zoom (View > Zoom), which the system's menus are placed by: the page's pixels are not the window's points.
    static ZOOM: Cell<f64> = const { Cell::new(1.0) };
}

/// The system's own menu for `items` (the original's `showContextMenu` over IPC), shown where the page's pixels say. It opens at once
/// and holds the interface until it closes, as a system menu does; the answer is the id chosen, or none when it was dismissed.
/// None here when this platform has no native menu to offer, and the page draws one itself.
pub(crate) fn native_menu(items: &[t3_client::context_menu::Item], at: Option<(f64, f64)>) -> Option<impl std::future::Future<Output = Option<String>>> {
    #[cfg(target_os = "macos")]
    {
        use dioxus::desktop::{muda::ContextMenu, tao::platform::macos::WindowExtMacOS};
        use t3_client::context_menu::{self as model, Native};
        let (entries, ids) = model::native(items);
        if entries.is_empty() {
            return Some(futures_util::future::Either::Left(std::future::ready(None)));
        }
        fn fill(menu: &dyn Fn(&dyn muda::IsMenuItem), entries: &[Native]) {
            for entry in entries {
                match entry {
                    Native::Separator => menu(&muda::PredefinedMenuItem::separator()),
                    Native::Leaf { index, label, enabled, checked: None } => menu(&muda::MenuItem::with_id(format!("{CONTEXT_ID}#{index}"), label, *enabled, None)),
                    Native::Leaf { index, label, enabled, checked: Some(checked) } => menu(&muda::CheckMenuItem::with_id(format!("{CONTEXT_ID}#{index}"), label, *enabled, *checked, None)),
                    Native::Submenu { label, enabled, entries } => {
                        let submenu = muda::Submenu::new(label, *enabled);
                        fill(&|item| {
                            let _ = submenu.append(item);
                        }, entries);
                        menu(&submenu)
                    }
                }
            }
        }
        let menu = muda::Menu::new();
        fill(&|item| {
            let _ = menu.append(item);
        }, &entries);
        let (reply, answer) = futures_channel::oneshot::channel::<Option<String>>();
        let reply = Rc::new(RefCell::new(Some(reply)));
        MENU_CHOICE.with(|choice| {
            let reply = reply.clone();
            *choice.borrow_mut() = Some(Box::new(move |id| {
                let chosen = id.strip_prefix('#').and_then(|index| index.parse::<usize>().ok()).and_then(|index| ids.get(index).cloned());
                if let Some(reply) = reply.borrow_mut().take() {
                    let _ = reply.send(chosen);
                }
            }));
        });
        let position = model::native_position(at, ZOOM.with(Cell::get)).map(|(x, y)| muda::dpi::Position::Logical(muda::dpi::LogicalPosition::new(x, y)));
        let view = dioxus::desktop::window().ns_view();
        // Returns when the menu closes; a choice arrives through the menu event handler, after this returns.
        unsafe { menu.show_context_menu_for_nsview(view, position) };
        // The choice was posted while the menu was up, and is handled before this timer fires; no choice by then is a dismissal.
        spawn(async move {
            runtime::delay(60).await;
            MENU_CHOICE.with(|choice| choice.borrow_mut().take());
            if let Some(reply) = reply.borrow_mut().take() {
                let _ = reply.send(None);
            }
        });
        Some(futures_util::future::Either::Right(async move { answer.await.ok().flatten() }))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (items, at);
        None::<std::future::Ready<Option<String>>>
    }
}

/// A notification through the system. macOS shows it through the system's `osascript`, as the app that runs it;
/// the original shows it as T3 Code, which needs a signed bundle this build does not have. Returns whether it was shown.
pub(crate) fn notify(title: &str, body: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        let script = format!("display notification {} with title {}", apple_script_string(body), apple_script_string(title));
        std::process::Command::new("/usr/bin/osascript").args(["-e", &script]).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().is_ok()
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (title, body);
        false
    }
}
#[cfg(target_os = "macos")]
fn apple_script_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The count on the dock icon, from the thread that owns the window: tao's window reaches the application's dock tile
/// only from there, which is where the interface runs. Nothing is shown for none.
pub(crate) fn set_badge(count: usize) {
    #[cfg(target_os = "macos")]
    {
        use dioxus::desktop::tao::platform::macos::WindowExtMacOS;
        dioxus::desktop::window().set_badge_label((count > 0).then(|| if count > 9 { "9+".to_owned() } else { count.to_string() }));
    }
    #[cfg(not(target_os = "macos"))]
    let _ = count;
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

/// The native save dialog (`dialog.showSaveDialog`), starting at the person's downloads folder. None if cancelled.
pub(crate) async fn pick_save_path(name: &str) -> Option<String> {
    let mut dialog = rfd::AsyncFileDialog::new().set_title("Save").set_file_name(name);
    if let Some(home) = env("HOME") {
        dialog = dialog.set_directory(PathBuf::from(home).join("Downloads"));
    }
    dialog.save_file().await.map(|file| file.path().to_string_lossy().into_owned())
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
