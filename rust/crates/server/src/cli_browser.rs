//! `t3 browser setup` (apps/server/src/cli/browser.ts): prepares a Linux host for T3's headless browser, which server browser tabs and HTML
//! render previews share. It is the fix every browser host error names, so it does the whole job in one run:
//!
//! - installs the AppArmor profile that lets Chrome's sandbox run where the host restricts unprivileged user namespaces (Ubuntu 23.10+), and
//! - installs the Debian packages for any libraries the browser cannot load.
//!
//! Both need root. Without it, the command prints what it would change and the `sudo` line to run. It is safe to run again; it skips what is
//! already done. Everything that touches the system (root, the profile file, `apt`, `ldd`) goes through `SetupHost`, so the steps are tested
//! against a fake and a test never runs `sudo` or installs a package.
use crate::{
    cli_commands::{Outcome, Spec, parse},
    preview_browser_host::{self, APPARMOR_PROFILE, APPARMOR_PROFILE_PATH, DEBIAN_PACKAGES},
};
use std::path::{Path, PathBuf};

const SETUP: Spec = Spec { values: &["base-dir"], booleans: &[], repeated: &[], positionals: (0, 0) };

/// What setup needs of the machine it runs on.
pub trait SetupHost {
    /// `std::env::consts::OS`.
    fn platform(&self) -> String;
    fn is_root(&self) -> bool;
    /// The command a person runs as root, as `sudo` needs it written.
    fn setup_command(&self) -> String;
    /// The T3 home to check for an installed browser.
    fn base_dir(&self, explicit: Option<&str>) -> Result<PathBuf, String>;
    /// Whether Chrome's sandbox will be blocked here.
    fn sandbox_blocked(&self) -> bool;
    fn installed_browser(&self, base_dir: &Path) -> Option<PathBuf>;
    fn missing_libraries(&self, executable: &Path) -> Vec<String>;
    fn has_apt(&self) -> bool;
    /// Writes the AppArmor profile at `APPARMOR_PROFILE_PATH`.
    fn write_profile(&self, content: &str) -> Result<(), String>;
    /// Runs one setup command with the person's terminal, and fails with its exit code.
    fn run_step(&self, command: &str, args: &[String]) -> Result<(), String>;
    /// Whether apt has an installable candidate for `package`.
    fn apt_offers(&self, package: &str) -> bool;
}

fn step_failed(step: &str, detail: impl std::fmt::Display) -> String {
    format!("Could not {step}: {detail}")
}

fn run_step(host: &dyn SetupHost, step: &str, command: &str, args: &[&str]) -> Result<(), String> {
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    host.run_step(command, &args).map_err(|detail| step_failed(step, detail))
}

/// The steps of `t3 browser setup`, printed as they happen. A failing step ends the command with its message.
pub fn setup(host: &dyn SetupHost, base_dir: Option<&str>) -> Outcome {
    let mut out = String::new();
    let mut log = |text: &str| {
        out.push_str(text);
        out.push('\n');
    };
    let result = (|| -> Result<(), String> {
        if host.platform() != "linux" {
            log("Nothing to set up: T3's browser runs as is on this system.");
            return Ok(());
        }
        let setup_command = host.setup_command();
        let needs_profile = host.sandbox_blocked();
        let browser = host.installed_browser(&host.base_dir(base_dir)?);
        let missing = browser.as_deref().map(|executable| host.missing_libraries(executable)).unwrap_or_default();
        if !needs_profile && missing.is_empty() {
            log(if browser.is_some() {
                "This host is ready for T3's browser."
            } else {
                "Chrome's sandbox is allowed here. T3's browser installs on first use; if it then reports missing libraries, run this again."
            });
            return Ok(());
        }
        if !host.is_root() {
            if needs_profile {
                log(&format!("This host blocks the sandbox T3's browser runs in. Setup installs an AppArmor profile at {APPARMOR_PROFILE_PATH} that allows it."));
            }
            if !missing.is_empty() {
                log(&format!("T3's browser is missing {}; setup installs them.", missing.join(", ")));
            }
            log(&format!("\nThis needs root. Run:\n\n  {setup_command}\n"));
            return Ok(());
        }
        if needs_profile {
            host.write_profile(APPARMOR_PROFILE).map_err(|detail| step_failed(&format!("write {APPARMOR_PROFILE_PATH}"), detail))?;
            run_step(host, "load the AppArmor profile", "apparmor_parser", &["-r", APPARMOR_PROFILE_PATH])?;
            log("Allowed Chrome's sandbox for T3's browser.");
        }
        if !missing.is_empty() {
            if !host.has_apt() {
                log(&format!("T3's browser is missing {}. Install them with your package manager, then run this again.", missing.join(", ")));
                return Ok(());
            }
            run_step(host, "refresh the package lists", "apt-get", &["update"])?;
            // Whichever name of each library apt offers.
            let packages: Vec<&str> = DEBIAN_PACKAGES.iter().map(|names| names.iter().copied().find(|name| host.apt_offers(name)).unwrap_or(names[0])).collect();
            let mut args = vec!["install", "-y", "--no-install-recommends"];
            args.extend(packages);
            run_step(host, "install the browser's libraries", "apt-get", &args)?;
            log("Installed the browser's libraries.");
        }
        log("This host is ready for T3's browser.");
        Ok(())
    })();
    match result {
        Ok(()) => Outcome { stdout: out, stderr: String::new(), code: 0 },
        Err(message) => Outcome { stdout: out, stderr: format!("{message}\n"), code: 1 },
    }
}

/// `t3 browser <subcommand>`.
pub fn browser_command(args: &[String], env: &dyn Fn(&str) -> Option<String>) -> Outcome {
    browser_command_on(args, &SystemHost { env_home: env("T3CODE_HOME"), sudo_user: env("SUDO_USER"), home: env("HOME") }, env)
}

pub fn browser_command_on(args: &[String], host: &dyn SetupHost, _env: &dyn Fn(&str) -> Option<String>) -> Outcome {
    match args.first().map(String::as_str) {
        Some("setup") => match parse(&args[1..], &SETUP) {
            Ok(parsed) => setup(host, parsed.value("base-dir").filter(|base| !base.trim().is_empty())),
            Err(message) => Outcome::failure(message),
        },
        Some(other) => Outcome::failure(format!("Unknown subcommand \"{other}\" for \"t3 browser\"")),
        None => Outcome::failure("Missing subcommand for \"t3 browser\""),
    }
}

/// The machine this process runs on.
struct SystemHost {
    env_home: Option<String>,
    sudo_user: Option<String>,
    home: Option<String>,
}

impl SetupHost for SystemHost {
    fn platform(&self) -> String {
        std::env::consts::OS.to_owned()
    }
    #[cfg(unix)]
    fn is_root(&self) -> bool {
        // SAFETY: `geteuid` takes no arguments and cannot fail.
        unsafe { libc::geteuid() == 0 }
    }
    #[cfg(not(unix))]
    fn is_root(&self) -> bool {
        false
    }
    fn setup_command(&self) -> String {
        crate::html_render::root_setup_command()
    }
    /// The T3 home to check. Under `sudo` the process home is root's, so an unspecified home falls back to the invoking user's `~/.t3`.
    fn base_dir(&self, explicit: Option<&str>) -> Result<PathBuf, String> {
        let env = |name: &str| match name {
            "HOME" => self.home.clone(),
            _ => None,
        };
        if let Some(explicit) = explicit.map(str::to_owned).or_else(|| self.env_home.clone().filter(|home| !home.trim().is_empty())) {
            return Ok(crate::cli_commands::expand_home(explicit.trim(), &env));
        }
        if let Some(user) = &self.sudo_user {
            let entry = std::process::Command::new("getent").args(["passwd", user]).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null()).output().ok();
            let home = entry.and_then(|output| String::from_utf8_lossy(&output.stdout).trim().split(':').nth(5).map(str::to_owned)).filter(|home| !home.is_empty());
            if let Some(home) = home {
                return Ok(PathBuf::from(home).join(".t3"));
            }
        }
        // The state directory a server started here would use, whose parent holds `tools`.
        Ok(crate::connect::base_dir_of(&std::env::current_dir().map_err(|error| error.to_string())?.join(".t3-rust")))
    }
    fn sandbox_blocked(&self) -> bool {
        tokio::runtime::Builder::new_current_thread().enable_all().build().is_ok_and(|runtime| runtime.block_on(preview_browser_host::sandbox_blocked()))
    }
    fn installed_browser(&self, base_dir: &Path) -> Option<PathBuf> {
        let root = base_dir.join("tools").join("chrome-headless-shell");
        for platform in std::fs::read_dir(&root).ok()?.flatten() {
            for version in std::fs::read_dir(platform.path()).into_iter().flatten().flatten() {
                let executable = version.path().join("chrome-headless-shell");
                if executable.exists() {
                    return Some(executable);
                }
            }
        }
        None
    }
    fn missing_libraries(&self, executable: &Path) -> Vec<String> {
        tokio::runtime::Builder::new_current_thread().enable_all().build().map(|runtime| runtime.block_on(preview_browser_host::missing_libraries(executable))).unwrap_or_default()
    }
    fn has_apt(&self) -> bool {
        Path::new("/usr/bin/apt-get").exists()
    }
    fn write_profile(&self, content: &str) -> Result<(), String> {
        std::fs::write(APPARMOR_PROFILE_PATH, content).map_err(|error| error.to_string())
    }
    fn run_step(&self, command: &str, args: &[String]) -> Result<(), String> {
        match std::process::Command::new(command).args(args).status() {
            Ok(status) if status.success() => Ok(()),
            Ok(status) => Err(format!("{command} exited with {}", status.code().map_or("a signal".to_owned(), |code| code.to_string()))),
            Err(error) => Err(error.to_string()),
        }
    }
    fn apt_offers(&self, package: &str) -> bool {
        let policy = std::process::Command::new("apt-cache").args(["policy", package]).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null()).output().ok();
        let policy = policy.map(|output| String::from_utf8_lossy(&output.stdout).into_owned()).unwrap_or_default();
        policy.lines().find_map(|line| line.trim().strip_prefix("Candidate:")).map(str::trim).is_some_and(|candidate| !candidate.is_empty() && candidate != "(none)")
    }
}

#[cfg(test)]
#[path = "cli_browser_tests.rs"]
mod tests;
