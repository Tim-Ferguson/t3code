//! `t3 browser setup` against a host the test plays: what it prints, writes and runs. No test runs `sudo`, installs a package or writes
//! outside a temporary directory.
use super::*;
use std::sync::Mutex;

#[derive(Default)]
struct FakeHost {
    platform: String,
    root: bool,
    blocked: bool,
    browser: Option<PathBuf>,
    missing: Vec<String>,
    apt: bool,
    offered: Vec<&'static str>,
    failing: Option<&'static str>,
    log: Mutex<Vec<String>>,
}

impl FakeHost {
    fn linux() -> Self {
        Self { platform: "linux".into(), apt: true, ..Default::default() }
    }
    fn record(&self, line: String) {
        self.log.lock().unwrap().push(line);
    }
    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

impl SetupHost for FakeHost {
    fn platform(&self) -> String {
        self.platform.clone()
    }
    fn is_root(&self) -> bool {
        self.root
    }
    fn setup_command(&self) -> String {
        "sudo /opt/t3/t3 browser setup".into()
    }
    fn base_dir(&self, explicit: Option<&str>) -> Result<PathBuf, String> {
        self.record(format!("base-dir {}", explicit.unwrap_or("-")));
        Ok(PathBuf::from("/home/me/.t3"))
    }
    fn sandbox_blocked(&self) -> bool {
        self.blocked
    }
    fn installed_browser(&self, _: &Path) -> Option<PathBuf> {
        self.browser.clone()
    }
    fn missing_libraries(&self, _: &Path) -> Vec<String> {
        self.missing.clone()
    }
    fn has_apt(&self) -> bool {
        self.apt
    }
    fn write_profile(&self, content: &str) -> Result<(), String> {
        self.record(format!("write {} ({} bytes)", APPARMOR_PROFILE_PATH, content.len()));
        assert_eq!(content, APPARMOR_PROFILE);
        Ok(())
    }
    fn run_step(&self, command: &str, args: &[String]) -> Result<(), String> {
        self.record(format!("run {command} {}", args.join(" ")));
        if self.failing == Some(command) { Err(format!("{command} exited with 100")) } else { Ok(()) }
    }
    fn apt_offers(&self, package: &str) -> bool {
        self.offered.contains(&package)
    }
}

fn run(host: &FakeHost, args: &[&str]) -> Outcome {
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    browser_command_on(&args, host, &|_| None)
}

#[test]
fn a_system_that_needs_nothing_says_so_and_changes_nothing() {
    let other = FakeHost { platform: "macos".into(), blocked: true, ..Default::default() };
    let outcome = run(&other, &["setup"]);
    assert_eq!((outcome.stdout.as_str(), outcome.code), ("Nothing to set up: T3's browser runs as is on this system.\n", 0));
    assert!(other.log().is_empty());

    let fresh = FakeHost::linux();
    assert_eq!(run(&fresh, &["setup"]).stdout, "Chrome's sandbox is allowed here. T3's browser installs on first use; if it then reports missing libraries, run this again.\n");
    let ready = FakeHost { browser: Some("/t3/tools/chrome-headless-shell/linux64/1/chrome-headless-shell".into()), ..FakeHost::linux() };
    assert_eq!(run(&ready, &["setup"]).stdout, "This host is ready for T3's browser.\n");
    assert_eq!(ready.log(), ["base-dir -"]);
}

#[test]
fn without_root_it_names_what_it_would_change_and_the_command_to_run() {
    let host = FakeHost { blocked: true, browser: Some("/b".into()), missing: vec!["libnss3.so".into(), "libgbm.so.1".into()], ..FakeHost::linux() };
    let outcome = run(&host, &["setup", "--base-dir", "/srv/t3"]);
    assert_eq!(outcome.code, 0);
    assert_eq!(
        outcome.stdout,
        format!(
            "This host blocks the sandbox T3's browser runs in. Setup installs an AppArmor profile at {APPARMOR_PROFILE_PATH} that allows it.\nT3's browser is missing libnss3.so, libgbm.so.1; setup installs them.\n\nThis needs root. Run:\n\n  sudo /opt/t3/t3 browser setup\n\n"
        )
    );
    // Only what was asked about was read: no file written, no command run.
    assert_eq!(host.log(), ["base-dir /srv/t3"]);
}

#[test]
fn as_root_it_writes_the_profile_loads_it_and_installs_each_library_under_the_name_apt_offers() {
    let host = FakeHost { root: true, blocked: true, browser: Some("/b".into()), missing: vec!["libnss3.so".into()], offered: vec!["libglib2.0-0", "libasound2t64"], ..FakeHost::linux() };
    let outcome = run(&host, &["setup"]);
    assert_eq!((outcome.code, outcome.stderr.as_str()), (0, ""));
    assert_eq!(outcome.stdout, "Allowed Chrome's sandbox for T3's browser.\nInstalled the browser's libraries.\nThis host is ready for T3's browser.\n");
    let log = host.log();
    assert_eq!(log[1], format!("write {APPARMOR_PROFILE_PATH} ({} bytes)", APPARMOR_PROFILE.len()));
    assert_eq!(log[2], format!("run apparmor_parser -r {APPARMOR_PROFILE_PATH}"));
    assert_eq!(log[3], "run apt-get update");
    let install = &log[4];
    assert!(install.starts_with("run apt-get install -y --no-install-recommends libnss3 "), "{install}");
    // The renamed package where apt offers it, the older name where it does not, and the first name when it offers neither.
    assert!(install.contains(" libglib2.0-0 ") && !install.contains("libglib2.0-0t64") && install.contains(" libasound2t64") && install.contains(" libatk1.0-0t64 "), "{install}");
}

#[test]
fn a_host_without_apt_is_told_to_use_its_package_manager() {
    let host = FakeHost { root: true, browser: Some("/b".into()), missing: vec!["libnss3.so".into()], apt: false, ..FakeHost::linux() };
    let outcome = run(&host, &["setup"]);
    assert_eq!(outcome.code, 0);
    assert_eq!(outcome.stdout, "T3's browser is missing libnss3.so. Install them with your package manager, then run this again.\n");
    assert_eq!(host.log(), ["base-dir -"]);
}

#[test]
fn a_step_that_fails_ends_the_command_with_what_failed() {
    let host = FakeHost { root: true, blocked: true, failing: Some("apparmor_parser"), ..FakeHost::linux() };
    let outcome = run(&host, &["setup"]);
    assert_eq!((outcome.code, outcome.stderr.as_str()), (1, "Could not load the AppArmor profile: apparmor_parser exited with 100\n"));
    // Nothing after the failed step ran.
    assert!(!host.log().iter().any(|line| line.contains("apt-get")));
    let apt = FakeHost { root: true, browser: Some("/b".into()), missing: vec!["libnss3.so".into()], failing: Some("apt-get"), ..FakeHost::linux() };
    assert_eq!(run(&apt, &["setup"]).stderr, "Could not refresh the package lists: apt-get exited with 100\n");
}

#[test]
fn the_subcommand_and_flags_are_checked() {
    let host = FakeHost::linux();
    assert_eq!(run(&host, &[]).stderr, "Missing subcommand for \"t3 browser\"\n");
    assert_eq!(run(&host, &["start"]).stderr, "Unknown subcommand \"start\" for \"t3 browser\"\n");
    assert_eq!(run(&host, &["setup", "--yes"]).stderr, "Unrecognized flag: --yes\n");
    assert_eq!(run(&host, &["setup", "extra"]).stderr, "Unexpected argument: extra\n");
    assert!(host.log().is_empty());
}

#[test]
fn the_command_is_reachable_from_the_cli_dispatch() {
    let args = vec!["browser".to_owned(), "nonsense".to_owned()];
    let outcome = crate::cli_commands::dispatch(&args, &|_| None).expect("`browser` is a command");
    assert_eq!(outcome.stderr, "Unknown subcommand \"nonsense\" for \"t3 browser\"\n");
}
