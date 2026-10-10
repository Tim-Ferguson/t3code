//! Opening a folder or file in the user's editor, and finding which editors are installed (port of
//! `process/externalLauncher.ts`, `shared/editor.ts` and the command lookup of `shared/shell.ts`).
//!
//! `shell.openInEditor` runs a program on the host, so the rules here are strict: the program is
//! one of a fixed table's commands (or a file found by walking fixed install folders), the target
//! is passed as one argument and never through a shell, and the process is detached with its
//! standard streams closed. Windows launches (cmd shims, PowerShell quoting for Explorer) follow
//! the original except that `.cmd`/`.bat` shims are spawned directly rather than through a shell.
use crate::provider_maintenance::{Platform, resolve_command_path};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LaunchStyle {
    DirectPath,
    Goto,
    LineColumn,
}

pub struct Editor {
    pub id: &'static str,
    pub label: &'static str,
    pub commands: Option<&'static [&'static str]>,
    pub base_args: &'static [&'static str],
    pub launch_style: LaunchStyle,
}

const fn editor(id: &'static str, label: &'static str, commands: &'static [&'static str], launch_style: LaunchStyle) -> Editor {
    Editor { id, label, commands: Some(commands), base_args: &[], launch_style }
}

/// `EDITORS`, in the order the original lists (and discovers) them.
pub static EDITORS: [Editor; 21] = [
    Editor { id: "cursor", label: "Cursor", commands: Some(&["cursor"]), base_args: &["--classic"], launch_style: LaunchStyle::Goto },
    editor("trae", "Trae", &["trae"], LaunchStyle::Goto),
    Editor { id: "kiro", label: "Kiro", commands: Some(&["kiro"]), base_args: &["ide"], launch_style: LaunchStyle::Goto },
    editor("vscode", "VS Code", &["code"], LaunchStyle::Goto),
    editor("vscode-insiders", "VS Code Insiders", &["code-insiders"], LaunchStyle::Goto),
    editor("vscodium", "VSCodium", &["codium"], LaunchStyle::Goto),
    editor("zed", "Zed", &["zed", "zeditor"], LaunchStyle::DirectPath),
    editor("antigravity", "Antigravity", &["antigravity-ide", "agy-ide"], LaunchStyle::Goto),
    editor("idea", "IntelliJ IDEA", &["idea"], LaunchStyle::LineColumn),
    editor("aqua", "Aqua", &["aqua"], LaunchStyle::LineColumn),
    editor("clion", "CLion", &["clion"], LaunchStyle::LineColumn),
    editor("datagrip", "DataGrip", &["datagrip"], LaunchStyle::LineColumn),
    editor("dataspell", "DataSpell", &["dataspell"], LaunchStyle::LineColumn),
    editor("goland", "GoLand", &["goland"], LaunchStyle::LineColumn),
    editor("phpstorm", "PhpStorm", &["phpstorm"], LaunchStyle::LineColumn),
    editor("pycharm", "PyCharm", &["pycharm"], LaunchStyle::LineColumn),
    editor("rider", "Rider", &["rider"], LaunchStyle::LineColumn),
    editor("rubymine", "RubyMine", &["rubymine"], LaunchStyle::LineColumn),
    editor("rustrover", "RustRover", &["rustrover"], LaunchStyle::LineColumn),
    editor("webstorm", "WebStorm", &["webstorm"], LaunchStyle::LineColumn),
    Editor { id: "file-manager", label: "File Manager", commands: None, base_args: &[], launch_style: LaunchStyle::DirectPath },
];

/// What lookups read: the process environment's relevant variables, and the folders an editor may
/// be installed in when it is not on `PATH`. Tests point the folders at temp directories.
#[derive(Clone)]
pub struct Probe {
    pub platform: Platform,
    pub env: HashMap<String, String>,
    /// macOS `/Applications`, searched after `~/Applications`.
    pub applications: Vec<PathBuf>,
    /// Linux directories searched after `~/.local/bin`.
    pub linux_dirs: Vec<PathBuf>,
}

const LOOKUP_ENV: [&str; 19] = [
    "SYSTEMROOT", "windir", "WSL_DISTRO_NAME", "WSL_INTEROP", "SSH_CONNECTION", "SSH_TTY", "container", "DISPLAY", "WAYLAND_DISPLAY",
    "PATH", "Path", "path", "PATHEXT", "HOME", "LOCALAPPDATA", "ProgramFiles", "ProgramW6432", "XDG_DATA_HOME", "ProgramFiles(x86)",
];

/// Set by `--fixture` (see `fixture`): the host then has no editors and no file manager.
static NO_EDITORS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// For a server started with `--fixture`: `Probe::host()` finds nothing, so no application of the machine is offered or launched.
pub(crate) fn offer_no_editors() {
    NO_EDITORS.store(true, std::sync::atomic::Ordering::Relaxed);
}

impl Probe {
    /// The real host: the variables the original reads, no others.
    pub fn host() -> Self {
        if NO_EDITORS.load(std::sync::atomic::Ordering::Relaxed) {
            return Self { platform: Platform::host(), env: HashMap::new(), applications: vec![], linux_dirs: vec![] };
        }
        Self {
            platform: Platform::host(),
            env: LOOKUP_ENV.iter().filter_map(|name| std::env::var(name).ok().map(|value| ((*name).to_owned(), value))).collect(),
            applications: vec![PathBuf::from("/Applications")],
            linux_dirs: ["/usr/local/bin", "/usr/bin", "/snap/bin"].into_iter().map(PathBuf::from).collect(),
        }
    }
    fn var(&self, name: &str) -> Option<&str> {
        self.env.get(name).map(String::as_str)
    }
    fn available(&self, command: &str) -> bool {
        resolve_command_path(command, &self.env, self.platform).is_some()
    }
}

// ---- command resolution (shared/editor.ts) -------------------------------------------------

fn install_names(id: &str, label: &str) -> Vec<String> {
    let names: &[&str] = match id {
        "vscode" => &["Visual Studio Code"],
        "vscode-insiders" => &["Visual Studio Code - Insiders"],
        // `Antigravity.app` is the separate Hub, not the IDE.
        "antigravity" => &["Antigravity IDE"],
        "idea" => &["IntelliJ IDEA", "IntelliJ IDEA CE", "IntelliJ IDEA Ultimate"],
        "pycharm" => &["PyCharm", "PyCharm CE"],
        "rider" => &["Rider", "JetBrains Rider"],
        _ => return vec![label.to_owned()],
    };
    names.iter().map(|name| (*name).to_owned()).collect()
}

/// `<name> <version>.app`, where the version is digits and dots. A plain prefix match would let one
/// editor steal another's bundle (stable VS Code resolving to the Insiders bundle).
fn is_versioned_bundle(entry: &str, name: &str) -> bool {
    let Some(rest) = entry.strip_prefix(&format!("{name} ")) else { return false };
    let Some(version) = rest.strip_suffix(".app") else { return false };
    version.chars().next().is_some_and(|c| c.is_ascii_digit()) && version.chars().all(|c| c.is_ascii_digit() || c == '.')
}

fn read_directory(path: &Path) -> Vec<String> {
    std::fs::read_dir(path).map(|entries| entries.flatten().map(|entry| entry.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default()
}

/// `resolveEditorCommand`: the first of the editor's commands on `PATH`, else the first executable
/// found in the folders its installer uses. `None` for the file manager and for an absent editor.
pub fn resolve_editor_command(editor: &Editor, probe: &Probe) -> Option<(String, Vec<String>)> {
    let commands = editor.commands?;
    let base_args: Vec<String> = editor.base_args.iter().map(|arg| (*arg).to_owned()).collect();
    for command in commands {
        if probe.available(command) {
            return Some(((*command).to_owned(), base_args));
        }
    }
    let home = probe.var("HOME").map(PathBuf::from);
    let names = install_names(editor.id, editor.label);
    let command = commands[0];
    let jetbrains = editor.launch_style == LaunchStyle::LineColumn;
    let mut candidates: Vec<PathBuf> = Vec::new();
    match probe.platform {
        Platform::Darwin => {
            let roots: Vec<PathBuf> = home.iter().map(|home| home.join("Applications")).chain(probe.applications.iter().cloned()).collect();
            for root in roots {
                // JetBrains Toolbox installs bundles named after the app and its version, so the
                // directory is listed and `<name> <version>.app` accepted beside `<name>.app`.
                let mut bundles: Vec<String> = names.iter().map(|name| format!("{name}.app")).collect();
                for entry in read_directory(&root) {
                    if names.iter().any(|name| entry == format!("{name}.app") || is_versioned_bundle(&entry, name)) && !bundles.contains(&entry) {
                        bundles.push(entry);
                    }
                }
                for bundle in bundles {
                    let contents = root.join(&bundle).join("Contents");
                    if jetbrains || editor.id == "zed" {
                        candidates.push(contents.join("MacOS").join(if editor.id == "zed" { "cli" } else { command }));
                    } else {
                        candidates.push(contents.join("Resources/app/bin").join(command));
                        candidates.push(contents.join("Resources/app/bin/code"));
                    }
                }
            }
            if let (Some(home), true) = (&home, jetbrains) {
                candidates.push(home.join("Library/Application Support/JetBrains/Toolbox/scripts").join(command));
            }
        }
        Platform::Win32 => {
            let mut roots: Vec<PathBuf> = probe.var("LOCALAPPDATA").map(|dir| vec![PathBuf::from(dir).join("Programs")]).unwrap_or_default();
            roots.extend(["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"].iter().filter_map(|name| probe.var(name).filter(|dir| !dir.is_empty()).map(PathBuf::from)));
            if jetbrains {
                if let Some(local) = probe.var("LOCALAPPDATA") {
                    candidates.push(PathBuf::from(local).join("JetBrains/Toolbox/scripts").join(format!("{command}.cmd")));
                }
                for directory in roots.iter().flat_map(|root| [root.clone(), root.join("JetBrains")]) {
                    for entry in read_directory(&directory) {
                        if names.iter().any(|name| entry == *name || entry.starts_with(&format!("{name} "))) {
                            candidates.push(directory.join(&entry).join("bin").join(format!("{command}64.exe")));
                            candidates.push(directory.join(&entry).join("bin").join(format!("{command}.exe")));
                        }
                    }
                }
            } else {
                let name = match editor.id {
                    "vscode" => "Microsoft VS Code",
                    "vscode-insiders" => "Microsoft VS Code Insiders",
                    _ => editor.label,
                };
                for root in &roots {
                    candidates.push(root.join(name).join("resources/app/bin").join(format!("{command}.cmd")));
                    candidates.push(root.join(name).join("resources/app/bin/code.cmd"));
                    candidates.push(root.join(name).join("bin").join(format!("{command}.cmd")));
                    candidates.push(root.join(name).join("bin/code.cmd"));
                    if editor.id == "zed" {
                        candidates.push(root.join(name).join("bin/zed.exe"));
                        candidates.push(root.join(name).join("zed.exe"));
                    }
                }
            }
        }
        Platform::Linux => {
            let mut dirs: Vec<PathBuf> = home.iter().map(|home| home.join(".local/bin")).chain(probe.linux_dirs.iter().cloned()).collect();
            if jetbrains {
                let data_home = probe.var("XDG_DATA_HOME").filter(|dir| !dir.is_empty()).map(PathBuf::from).or_else(|| home.as_ref().map(|home| home.join(".local/share")));
                if let Some(data_home) = data_home {
                    dirs.push(data_home.join("JetBrains/Toolbox/scripts"));
                }
            }
            for dir in dirs {
                for name in commands {
                    candidates.push(dir.join(name));
                }
            }
        }
    }
    for candidate in candidates {
        let text = candidate.to_string_lossy().into_owned();
        if probe.available(&text) {
            let keeps_base = !(editor.id == "kiro" && matches!(probe.platform, Platform::Darwin | Platform::Win32));
            return Some((text, if keeps_base { base_args } else { Vec::new() }));
        }
    }
    None
}

// ---- file manager --------------------------------------------------------------------------

fn uses_windows_host_from_wsl(probe: &Probe) -> bool {
    probe.platform == Platform::Linux
        && (probe.var("WSL_DISTRO_NAME").is_some() || probe.var("WSL_INTEROP").is_some())
        && probe.var("SSH_CONNECTION").is_none()
        && probe.var("SSH_TTY").is_none()
        && probe.var("container").is_none()
}
fn has_graphical_session(probe: &Probe) -> bool {
    ["DISPLAY", "WAYLAND_DISPLAY"].iter().any(|name| probe.var(name).is_some_and(|value| !value.trim().is_empty()))
}
fn file_manager_command(probe: &Probe) -> Option<&'static str> {
    match probe.platform {
        Platform::Darwin => Some("open"),
        Platform::Win32 => Some("explorer"),
        Platform::Linux if uses_windows_host_from_wsl(probe) => probe.var("WSL_DISTRO_NAME").is_some_and(|name| !name.trim().is_empty()).then_some("explorer.exe"),
        Platform::Linux => has_graphical_session(probe).then_some("xdg-open"),
    }
}

/// A graphical session and an executable `xdg-open` do not prove opening a directory does
/// anything: without an `inode/directory` handler `xdg-open` exits nonzero after the launcher has
/// detached, a silent no-op. The probe has its own timeout so a hung `xdg-mime` costs only the
/// file manager, not every discovered editor.
fn has_linux_directory_handler(probe: &Probe) -> bool {
    use std::process::{Command, Stdio};
    if !probe.available("xdg-mime") {
        return false;
    }
    let Some(program) = resolve_command_path("xdg-mime", &probe.env, probe.platform) else { return false };
    let Ok(mut child) = Command::new(program).args(["query", "default", "inode/directory"]).stdin(Stdio::null()).stderr(Stdio::null()).stdout(Stdio::piped()).spawn() else { return false };
    let Some(mut stdout) = child.stdout.take() else { return false };
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut stdout, &mut text).ok().map(|_| text)
    });
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success() && reader.join().ok().flatten().is_some_and(|text| !text.trim().is_empty()),
            Ok(None) if started.elapsed() > Duration::from_secs(2) => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(_) => return false,
        }
    }
}
fn usable_file_manager_command(command: &str, probe: &Probe) -> bool {
    probe.available(command) && (command != "xdg-open" || has_linux_directory_handler(probe))
}
/// The file-manager command a launch can actually run, not just the platform's preference.
fn resolve_usable_file_manager_command(probe: &Probe) -> Option<&'static str> {
    let preferred = file_manager_command(probe);
    if let Some(command) = preferred.filter(|command| usable_file_manager_command(command, probe)) {
        return Some(command);
    }
    (uses_windows_host_from_wsl(probe) && has_graphical_session(probe) && usable_file_manager_command("xdg-open", probe)).then_some("xdg-open")
}

fn powershell_path(probe: &Probe) -> String {
    format!("{}\\System32\\WindowsPowerShell\\v1.0\\powershell.exe", probe.var("SYSTEMROOT").filter(|v| !v.is_empty()).or_else(|| probe.var("windir").filter(|v| !v.is_empty())).unwrap_or("C:\\Windows"))
}
const WSL_POWERSHELL_COMMAND: &str = "powershell.exe";

/// Which file manager the host has, as a reveal kind, or `None` when what a reveal spawns is missing.
pub fn file_manager_reveal_kind(probe: &Probe) -> Option<&'static str> {
    match probe.platform {
        Platform::Darwin => Some("finder"),
        Platform::Win32 => probe.available(&powershell_path(probe)).then_some("file-explorer"),
        Platform::Linux if uses_windows_host_from_wsl(probe) => {
            if probe.var("WSL_DISTRO_NAME").is_some_and(|name| !name.trim().is_empty()) && probe.available("explorer.exe") && probe.available(WSL_POWERSHELL_COMMAND) {
                return Some("file-explorer");
            }
            (has_graphical_session(probe) && usable_file_manager_command("xdg-open", probe)).then_some("files")
        }
        Platform::Linux => has_graphical_session(probe).then_some("files"),
    }
}

/// The ids of the editors this host has, in table order (`buildAvailableEditors`).
pub fn available_editors(probe: &Probe) -> Vec<&'static str> {
    EDITORS
        .iter()
        .filter(|editor| match editor.commands {
            None => resolve_usable_file_manager_command(probe).is_some(),
            Some(_) => resolve_editor_command(editor, probe).is_some(),
        })
        .map(|editor| editor.id)
        .collect()
}

static DISCOVERY: Mutex<Option<(Instant, Vec<&'static str>)>> = Mutex::new(None);
const DISCOVERY_TTL: Duration = Duration::from_secs(60);

/// Editor discovery for `server.getConfig`: every connect asks, a PATH walk per editor is not free,
/// so the answer is kept for a minute. A scan that outlives the caller's timeout keeps running.
pub async fn discover_for_config() -> Vec<&'static str> {
    if let Some((at, found)) = DISCOVERY.lock().unwrap().as_ref() {
        if at.elapsed() < DISCOVERY_TTL {
            return found.clone();
        }
    }
    let scan = tokio::task::spawn_blocking(|| {
        let found = available_editors(&Probe::host());
        *DISCOVERY.lock().unwrap() = Some((Instant::now(), found.clone()));
        found
    });
    // The scan is not cancelled when this times out: the next ask finds its answer.
    tokio::time::timeout(Duration::from_secs(5), scan).await.ok().and_then(Result::ok).unwrap_or_default()
}

/// The editor-dependent fields of the config snapshot (`resolveEditorConfig`).
pub async fn overlay_config(config: &mut Value) {
    let editors = discover_for_config().await;
    let reveal = if editors.contains(&"file-manager") {
        tokio::time::timeout(Duration::from_secs(5), tokio::task::spawn_blocking(|| file_manager_reveal_kind(&Probe::host()))).await.ok().and_then(Result::ok).flatten()
    } else {
        None
    };
    let Some(object) = config.as_object_mut() else { return };
    object.insert("availableEditors".into(), json!(editors));
    object.remove("shellRevealInFileManager");
    object.remove("shellRevealInFileManagerKind");
    if let Some(kind) = reveal {
        object.insert("shellRevealInFileManager".into(), json!(true));
        object.insert("shellRevealInFileManagerKind".into(), json!(kind));
    }
}

// ---- building a launch ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub editor: &'static str,
    pub target: String,
    pub command: String,
    pub args: Vec<String>,
}

/// `path:line` or `path:line:column`.
fn target_and_position(target: &str) -> Option<(String, String, Option<String>)> {
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    let (rest, last) = target.rsplit_once(':')?;
    if !digits(last) {
        return None;
    }
    // `^(.*?):(\d+)(?::(\d+))?$` prefers the shortest path: with two numeric tails the first is the line.
    if let Some((path, line)) = rest.rsplit_once(':') {
        if digits(line) {
            return (!path.is_empty()).then(|| (path.to_owned(), line.to_owned(), Some(last.to_owned())));
        }
    }
    (!rest.is_empty()).then(|| (rest.to_owned(), last.to_owned(), None))
}

fn command_editor_args(editor: &Editor, target: &str) -> Vec<String> {
    let parsed = target_and_position(target);
    match editor.launch_style {
        LaunchStyle::DirectPath => vec![target.to_owned()],
        LaunchStyle::Goto => if parsed.is_some() { vec!["--goto".into(), target.to_owned()] } else { vec![target.to_owned()] },
        LaunchStyle::LineColumn => match parsed {
            None => vec![target.to_owned()],
            Some((path, line, column)) => {
                let mut args = vec!["--line".to_owned(), line];
                if let Some(column) = column {
                    args.extend(["--column".to_owned(), column]);
                }
                args.push(path);
                args
            }
        },
    }
}

fn utf16le_base64(input: &str) -> String {
    use base64::Engine;
    let bytes: Vec<u8> = input.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}
fn powershell_literal(input: &str) -> String {
    format!("'{}'", input.replace('\'', "''"))
}
const POWERSHELL_PREFIX: [&str; 5] = ["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-EncodedCommand"];

/// PowerShell source that launches File Explorer with its raw selection switch, whose contract is
/// the single argument `/select,"<path>"`; ordinary quoting makes Explorer open a fallback folder.
pub fn file_explorer_reveal_source(explorer: &str, target: &str) -> String {
    format!("$ProgressPreference = 'SilentlyContinue'; Start-Process {} -ArgumentList ('/select,\"' + {} + '\"')", powershell_literal(explorer), powershell_literal(target))
}
fn file_explorer_reveal(target: &str, explorer_target: &str, powershell: &str) -> Launch {
    let mut args: Vec<String> = POWERSHELL_PREFIX.iter().map(|arg| (*arg).to_owned()).collect();
    args.push(utf16le_base64(&file_explorer_reveal_source("explorer.exe", explorer_target)));
    Launch { editor: "file-manager", target: target.to_owned(), command: powershell.to_owned(), args }
}
fn wsl_file_manager_path(target: &str, distro: &str) -> String {
    let relative = target.trim_start_matches('/').replace('/', "\\");
    format!("\\\\wsl.localhost\\{distro}{}", if relative.is_empty() { String::new() } else { format!("\\{relative}") })
}
fn parent_of(target: &str) -> String {
    crate::asset_workspace::dirname(target)
}

fn reveal_launch(target: &str, probe: &Probe, command: &str) -> Launch {
    let simple = |command: &str, args: Vec<String>| Launch { editor: "file-manager", target: target.to_owned(), command: command.to_owned(), args };
    match probe.platform {
        Platform::Darwin => return simple("open", vec!["-R".into(), target.to_owned()]),
        Platform::Win32 => return file_explorer_reveal(target, &target.replace('/', "\\"), &powershell_path(probe)),
        Platform::Linux => {}
    }
    if let (true, true, Some(distro)) = (command == "explorer.exe", uses_windows_host_from_wsl(probe), probe.var("WSL_DISTRO_NAME")) {
        let explorer_target = wsl_file_manager_path(target, distro);
        if probe.available(WSL_POWERSHELL_COMMAND) {
            // Explorer's raw switch cannot express a double quote, and a WSL path may legally contain
            // one: open the containing directory instead.
            if explorer_target.contains('"') {
                return simple("explorer.exe", vec![wsl_file_manager_path(&parent_of(target), distro)]);
            }
            return file_explorer_reveal(target, &explorer_target, WSL_POWERSHELL_COMMAND);
        }
        if has_graphical_session(probe) && usable_file_manager_command("xdg-open", probe) {
            return simple("xdg-open", vec![parent_of(target)]);
        }
        return simple("explorer.exe", vec![wsl_file_manager_path(&parent_of(target), distro)]);
    }
    // Linux file managers have no portable "select this file" flag, so open the containing directory.
    simple(command, vec![parent_of(target)])
}

/// Why a launch failed, in the original's error members.
#[derive(Debug)]
pub enum LaunchError {
    UnknownEditor(String),
    UnsupportedEditor(&'static str),
    CommandNotFound { editor: &'static str, command: String },
    Spawn { editor: &'static str, target: String, command: String, args: Vec<String>, cause: String },
}

impl LaunchError {
    pub fn wire(&self) -> Value {
        match self {
            Self::UnknownEditor(editor) => json!({"_tag":"ExternalLauncherUnknownEditorError","editor":editor}),
            Self::UnsupportedEditor(editor) => json!({"_tag":"ExternalLauncherUnsupportedEditorError","editor":editor}),
            Self::CommandNotFound { editor, command } => json!({"_tag":"ExternalLauncherCommandNotFoundError","editor":editor,"command":command}),
            Self::Spawn { editor, target, command, args, cause } => {
                json!({"_tag":"ExternalLauncherEditorSpawnError","command":command,"args":args,"editor":editor,"target":target,"cause":{"name":"Error","message":cause}})
            }
        }
    }
}

/// `resolveEditorLaunch`: the command and arguments that open `cwd` in `editor_id`.
pub fn resolve_launch(editor_id: &str, cwd: &str, reveal: bool, probe: &Probe) -> Result<Launch, LaunchError> {
    let editor = EDITORS.iter().find(|editor| editor.id == editor_id).ok_or_else(|| LaunchError::UnknownEditor(editor_id.to_owned()))?;
    if let Some(commands) = editor.commands {
        let (command, base_args) = resolve_editor_command(editor, probe).unwrap_or_else(|| (commands[0].to_owned(), editor.base_args.iter().map(|arg| (*arg).to_owned()).collect()));
        let mut args = base_args;
        args.extend(command_editor_args(editor, cwd));
        return Ok(Launch { editor: editor.id, target: cwd.to_owned(), command, args });
    }
    let command = resolve_usable_file_manager_command(probe).ok_or(LaunchError::UnsupportedEditor(editor.id))?;
    if reveal {
        return Ok(reveal_launch(cwd, probe, command));
    }
    let args = match (command, probe.var("WSL_DISTRO_NAME")) {
        ("explorer.exe", Some(distro)) => vec![wsl_file_manager_path(cwd, distro)],
        _ => vec![cwd.to_owned()],
    };
    Ok(Launch { editor: editor.id, target: cwd.to_owned(), command: command.to_owned(), args })
}

/// Launches the editor as a detached process with its standard streams closed, so neither server
/// startup nor the request waits on it. The command must be available first.
pub fn launch(launch: &Launch, probe: &Probe) -> Result<(), LaunchError> {
    if !probe.available(&launch.command) {
        return Err(LaunchError::CommandNotFound { editor: launch.editor, command: launch.command.clone() });
    }
    let spawn_error = |error: std::io::Error| LaunchError::Spawn { editor: launch.editor, target: launch.target.clone(), command: launch.command.clone(), args: launch.args.clone(), cause: error.to_string() };
    let mut command = std::process::Command::new(&launch.command);
    command.args(&launch.args).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(spawn_error)?;
    // The launcher is not waited on, but a finished child must not stay a zombie.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// `shell.openInEditor`.
pub async fn open_in_editor(editor: String, cwd: String, reveal: bool) -> Result<(), LaunchError> {
    tokio::task::spawn_blocking(move || {
        let probe = Probe::host();
        let resolved = resolve_launch(&editor, &cwd, reveal, &probe)?;
        launch(&resolved, &probe)
    })
    .await
    .unwrap_or_else(|error| Err(LaunchError::Spawn { editor: "file-manager", target: String::new(), command: String::new(), args: vec![], cause: error.to_string() }))
}

pub const METHODS: [&str; 1] = ["shell.openInEditor"];

/// The working directory (trimmed), the editor and whether to reveal instead of open, from a `shell.openInEditor` payload.
fn decode_launch(payload: &Value) -> Result<(&str, &str, bool), Value> {
    let decode = |message: &str| json!({"_tag":"SchemaDecodeError","message":message});
    let cwd = payload["cwd"].as_str().map(t3_contracts::trim_wire_string).filter(|cwd| !cwd.is_empty()).ok_or_else(|| decode("cwd must be a non-empty string."))?;
    let editor = payload["editor"].as_str().filter(|id| EDITORS.iter().any(|editor| editor.id == *id)).ok_or_else(|| decode("editor must name a known editor."))?;
    let reveal = match payload.get("reveal") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(reveal)) => *reveal,
        Some(_) => return Err(decode("reveal must be a boolean.")),
    };
    Ok((cwd, editor, reveal))
}
pub(crate) fn decode_open(payload: &Value) -> Result<(), Value> {
    decode_launch(payload).map(|_| ())
}

/// Decode, one launch, the original's error envelopes.
pub async fn command(tag: &str, payload: Value) -> Result<Value, Value> {
    if tag != "shell.openInEditor" {
        return Err(json!({"_tag":"NativeMethodUnsupportedError","method":tag}));
    }
    let (cwd, editor, reveal) = decode_launch(&payload)?;
    open_in_editor(editor.to_owned(), cwd.to_owned(), reveal).await.map(|()| Value::Null).map_err(|error| error.wire())
}
