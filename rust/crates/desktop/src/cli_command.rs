//! Settings → `t3` command: puts the app's bundled command line (`t3-server`) on the person's PATH, or takes it off again. Port of
//! `DesktopCliCommand.ts` and `DesktopCliShim.ts` for macOS and Linux; Windows (a PATH entry in the registry) is not ported, so
//! the row is not offered there.
//!
//! The app keeps a small launcher for the bundled server in its own folder, which is never on PATH and so never shadows another
//! `t3`. Installing links it into a folder on the person's PATH that they can write without administrator rights, or into one
//! that is not yet on it. Removing undoes only what installing did: a link to a launcher the app wrote, found by the marker the
//! launcher carries (so a link made under an earlier data folder is cleaned up too, and a `t3` that is not ours is never touched).
//! Nothing here runs the program; the caller passes the folders, so a test works in a temporary directory.
use std::{
    fs,
    path::{Path, PathBuf},
};

/// What the launcher says about itself; a file without it is somebody else's.
pub const MARKER: &str = "Written by T3 Code: runs the desktop app's bundled t3 command line.";
/// The launcher is a few lines; a larger file behind a link is never read.
const LAUNCHER_MAX_BYTES: u64 = 16_384;

/// Where things are: the caller reads these from the machine, a test makes them in a temporary directory.
#[derive(Debug, Clone)]
pub struct Environment {
    /// The folder the app keeps its launcher in (`<app data>/bin`).
    pub launcher_dir: PathBuf,
    /// The bundled command line the launcher runs.
    pub executable: PathBuf,
    /// The server state directory of this app, named in the launcher for whoever wants to point a command at it.
    pub state_dir: PathBuf,
    /// Folders a link may go in, most wanted first.
    pub candidates: Vec<PathBuf>,
    /// The folders of `PATH`, in order.
    pub path: Vec<PathBuf>,
    /// Whether this build is a packaged one: a build run from a checkout has no command of its own to install.
    pub packaged: bool,
}

impl Environment {
    /// The machine's: the folders login shells commonly put on PATH that a person can write to, as the original lists them.
    pub fn candidates(home: &Path, macos: bool) -> Vec<PathBuf> {
        if macos {
            vec![PathBuf::from("/opt/homebrew/bin"), PathBuf::from("/usr/local/bin"), home.join(".local/bin"), home.join("bin")]
        } else {
            vec![home.join(".local/bin"), home.join("bin")]
        }
    }
    pub fn launcher(&self) -> PathBuf {
        self.launcher_dir.join("t3")
    }
}

/// What the row shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    /// Whether this machine and build can have the command at all; the row is hidden when not.
    pub supported: bool,
    /// The link the app made, if one exists.
    pub installed_path: Option<PathBuf>,
    /// Whether a new terminal would run it: the first `t3` on PATH is ours.
    pub on_path: bool,
}

fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\"'\"'"))
}

/// The launcher script (`renderCliShim`, direct kind): runs the bundled command line, and says where it is when the app has been moved.
pub fn render_launcher(executable: &Path, launcher: &Path, state_dir: &Path) -> String {
    let executable = executable.to_string_lossy();
    format!(
        "#!/bin/sh\n# {MARKER}\n# This app's server state, for commands that act on it: --state-dir {state}\nexport T3CODE_CLI_PATH={launcher}\napp={app}\nif [ ! -x \"$app\" ]; then\n  echo \"T3 Code has moved or been removed. Open the app once to update this command.\" >&2\n  exit 127\nfi\nexec \"$app\" \"$@\"\n",
        state = quoted(&state_dir.to_string_lossy()),
        launcher = quoted(&launcher.to_string_lossy()),
        app = quoted(&executable),
    )
}

/// A link to one of the app's launchers, by the marker the launcher carries.
fn is_ours(link: &Path) -> bool {
    let Ok(target) = fs::read_link(link) else { return false };
    let Ok(info) = fs::metadata(link) else { return false };
    let _ = target;
    info.is_file() && info.len() <= LAUNCHER_MAX_BYTES && fs::read_to_string(link).is_ok_and(|content| content.contains(MARKER))
}

fn installed_at(environment: &Environment) -> Option<PathBuf> {
    environment.candidates.iter().map(|directory| directory.join("t3")).find(|link| is_ours(link))
}

/// The `t3` a new shell runs, by PATH order.
fn first_on_path(environment: &Environment) -> Option<PathBuf> {
    environment.path.iter().map(|directory| directory.join("t3")).find(|candidate| fs::symlink_metadata(candidate).is_ok())
}

pub fn state(environment: &Environment) -> State {
    if !environment.packaged || cfg!(windows) {
        return State { supported: false, installed_path: None, on_path: false };
    }
    let Some(installed) = installed_at(environment) else { return State { supported: true, installed_path: None, on_path: false } };
    let on_path = first_on_path(environment).is_some_and(|first| is_ours(&first));
    State { supported: true, installed_path: Some(installed), on_path }
}

/// Writes the launcher if it is not there or has changed, beside the old one and renamed over it so a running `t3` never reads half a file.
/// A file the app did not write is left alone.
fn ensure_launcher(environment: &Environment) -> Result<PathBuf, String> {
    let launcher = environment.launcher();
    let content = render_launcher(&environment.executable, &launcher, &environment.state_dir);
    let existing = fs::read_to_string(&launcher).ok();
    if existing.as_deref().is_some_and(|existing| !existing.contains(MARKER)) {
        return Err(format!("Could not set up the t3 launcher at {}: a file the app did not write is there.", launcher.display()));
    }
    if existing.as_deref() != Some(content.as_str()) {
        let setup = || -> std::io::Result<()> {
            fs::create_dir_all(&environment.launcher_dir)?;
            let staging = launcher.with_extension(format!("{}.tmp", std::process::id()));
            fs::write(&staging, &content)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&staging, fs::Permissions::from_mode(0o755))?;
            }
            fs::rename(&staging, &launcher)
        };
        setup().map_err(|error| format!("Could not set up the t3 launcher at {}: {error}", launcher.display()))?;
    }
    Ok(launcher)
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}
#[cfg(not(unix))]
fn symlink(_target: &Path, _link: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other("links are not made here"))
}

fn writable_directory(directory: &Path) -> bool {
    fs::metadata(directory).is_ok_and(|info| info.is_dir() && !info.permissions().readonly()) && {
        // A directory can be listed as writable and still refuse a new entry; ask by trying.
        let probe = directory.join(format!(".t3-write-test-{}", std::process::id()));
        let wrote = fs::write(&probe, b"").is_ok();
        let _ = fs::remove_file(&probe);
        wrote
    }
}

/// Links the launcher into a folder on PATH.
pub fn install(environment: &Environment) -> Result<State, String> {
    if !environment.packaged || cfg!(windows) {
        return Err("The t3 command needs an installed app.".into());
    }
    let launcher = ensure_launcher(environment)?;
    if let Some(existing) = installed_at(environment) {
        if fs::read_link(&existing).ok().as_deref() == Some(launcher.as_path()) {
            return Ok(state(environment));
        }
        // A link to an earlier data folder's launcher: point it at this one instead.
        fs::remove_file(&existing).map_err(|_| format!("Could not replace {}.", existing.display()))?;
    }
    // Prefer a folder already on PATH that the person can write to without administrator rights.
    let on_path = |directory: &&PathBuf| environment.path.contains(directory);
    let ordered = environment.candidates.iter().filter(on_path).chain(environment.candidates.iter().filter(|directory| !on_path(directory)));
    for directory in ordered {
        let usable = if directory.exists() { writable_directory(directory) } else { fs::create_dir_all(directory).is_ok() };
        if !usable {
            continue;
        }
        // A link cannot be made where anything, even a broken link, is already.
        if symlink(&launcher, &directory.join("t3")).is_ok() {
            return Ok(state(environment));
        }
    }
    Err(format!("Another t3 command is already installed, or no folder on your PATH is writable. Run the launcher directly at {}.", launcher.display()))
}

/// Removes the links that point at one of the app's launchers.
pub fn uninstall(environment: &Environment) -> Result<State, String> {
    for link in environment.candidates.iter().map(|directory| directory.join("t3")) {
        if is_ours(&link) {
            fs::remove_file(&link).map_err(|_| format!("Could not remove {}.", link.display()))?;
        }
    }
    Ok(state(environment))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    struct Machine {
        _root: tempfile::TempDir,
        environment: Environment,
    }
    fn machine(packaged: bool) -> Machine {
        let root = tempfile::Builder::new().prefix("t3code-rust-port-test-cli-command-").tempdir().unwrap();
        let (first, second, app) = (root.path().join("first-bin"), root.path().join("second-bin"), root.path().join("app"));
        for directory in [&first, &second, &app.join("bin")] {
            fs::create_dir_all(directory).unwrap();
        }
        let executable = app.join("t3-server");
        fs::write(&executable, "#!/bin/sh\necho t3\n").unwrap();
        Machine {
            environment: Environment { launcher_dir: app.join("bin"), executable, state_dir: app.join("server"), candidates: vec![first.clone(), second.clone()], path: vec![second, first], packaged },
            _root: root,
        }
    }

    #[test]
    fn installing_links_a_launcher_into_a_folder_on_path_and_removing_takes_only_that_link_away() {
        let machine = machine(true);
        let environment = &machine.environment;
        assert_eq!(state(environment), State { supported: true, installed_path: None, on_path: false });

        let installed = install(environment).unwrap();
        // Of the folders on PATH, the first writable one that the candidates list names first.
        let link = environment.candidates[0].join("t3");
        assert_eq!(installed.installed_path.as_deref(), Some(link.as_path()));
        assert!(installed.on_path, "a new terminal finds our t3 first");
        assert_eq!(fs::read_link(&link).unwrap(), environment.launcher());
        let launcher = fs::read_to_string(environment.launcher()).unwrap();
        assert!(launcher.contains(MARKER) && launcher.contains("exec \"$app\" \"$@\""));
        assert!(launcher.contains(&environment.executable.to_string_lossy().to_string()));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(environment.launcher()).unwrap().permissions().mode() & 0o111, 0o111, "the launcher runs");

        // Installing again changes nothing.
        assert_eq!(install(environment).unwrap(), installed);

        // A t3 that is not ours is never touched, and one that shadows ours is said so.
        let other = environment.candidates[1].join("t3");
        fs::write(&other, "#!/bin/sh\necho someone else\n").unwrap();
        let removed = uninstall(environment).unwrap();
        assert_eq!(removed, State { supported: true, installed_path: None, on_path: false });
        assert!(!link.exists() && fs::symlink_metadata(&link).is_err(), "our link is gone");
        assert_eq!(fs::read_to_string(&other).unwrap(), "#!/bin/sh\necho someone else\n", "the other t3 is untouched");
        assert!(environment.launcher().exists(), "the launcher stays for the server to name by absolute path");
    }

    #[test]
    fn a_folder_that_already_holds_another_t3_is_skipped_and_a_full_set_says_so() {
        let machine = machine(true);
        let environment = &machine.environment;
        fs::write(environment.candidates[0].join("t3"), "not ours").unwrap();
        let installed = install(environment).unwrap();
        assert_eq!(installed.installed_path.as_deref(), Some(environment.candidates[1].join("t3").as_path()));
        // The first on PATH is the other one's `t3`... ours is first on PATH here (second-bin leads), so it runs.
        assert!(installed.on_path);
        uninstall(environment).unwrap();
        fs::write(environment.candidates[1].join("t3"), "not ours either").unwrap();
        let error = install(environment).unwrap_err();
        assert!(error.starts_with("Another t3 command is already installed"), "{error}");
        assert_eq!(fs::read_to_string(environment.candidates[0].join("t3")).unwrap(), "not ours");
    }

    #[test]
    fn a_link_made_under_an_earlier_data_folder_is_moved_to_this_launcher_and_a_build_without_a_package_has_none() {
        let machine = machine(true);
        let environment = &machine.environment;
        let earlier = machine._root.path().join("earlier-t3");
        fs::write(&earlier, render_launcher(Path::new("/gone/t3-server"), &earlier, Path::new("/gone/server"))).unwrap();
        symlink(&earlier, &environment.candidates[0].join("t3")).unwrap();
        assert!(state(environment).installed_path.is_some(), "the marker finds links to any of the app's launchers");
        let moved = install(environment).unwrap();
        assert_eq!(fs::read_link(moved.installed_path.unwrap()).unwrap(), environment.launcher());

        let unpackaged = Machine { environment: Environment { packaged: false, ..machine.environment.clone() }, _root: tempfile::tempdir().unwrap() };
        assert_eq!(state(&unpackaged.environment), State { supported: false, installed_path: None, on_path: false });
        assert!(install(&unpackaged.environment).is_err());
    }

    #[test]
    fn a_launcher_for_an_app_that_has_moved_says_so_instead_of_failing_silently() {
        let machine = machine(true);
        let script = render_launcher(Path::new("/definitely/not/here/t3-server"), &machine.environment.launcher(), &machine.environment.state_dir);
        let path = machine.environment.launcher();
        fs::write(&path, script).unwrap();
        let output = std::process::Command::new("/bin/sh").arg(&path).arg("--version").env_clear().output().unwrap();
        assert_eq!(output.status.code(), Some(127));
        assert!(String::from_utf8_lossy(&output.stderr).contains("T3 Code has moved or been removed"));
    }

    #[test]
    fn a_launcher_runs_the_bundled_program_with_its_arguments() {
        let machine = machine(true);
        let environment = &machine.environment;
        fs::write(&environment.executable, "#!/bin/sh\necho \"args:$*\"\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&environment.executable, fs::Permissions::from_mode(0o755)).unwrap();
        install(environment).unwrap();
        let output = std::process::Command::new(environment.candidates[0].join("t3")).args(["pair", "--ttl", "5m"]).env_clear().output().unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "args:pair --ttl 5m");
    }
}
