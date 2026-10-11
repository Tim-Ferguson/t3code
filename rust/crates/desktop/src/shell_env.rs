//! The login shell's environment for the server this app starts (`DesktopShellEnvironment.ts`). A
//! program started from the Finder or the dock gets launchd's bare environment, so the server it
//! spawns would not find the provider tools the person's own terminal finds. This asks the person's
//! login shell for the variables that matter, the way the original does, and returns the
//! environment the server should be started with.
//!
//! The shell is run as `shell -ilc '<one printf/printenv pair per name between markers>'` with a
//! five second limit (a terminate signal at the limit, then a kill one second later). A shell that
//! fails, hangs or prints nothing contributes nothing, and the next candidate (`$SHELL`, then
//! `/bin/zsh` on macOS or `/bin/bash` on Linux) is tried until one supplies `PATH`. On macOS,
//! without a `PATH` from any shell, `launchctl getenv PATH` is asked instead. The result is
//! computed once per run of the app and reused for each start of the server.
//!
//! Windows is not ported: its probes are PowerShell's, and this app cannot start the server there
//! yet (the bootstrap descriptor), so the environment passes through unchanged.
use std::{
    collections::BTreeMap,
    io::Read,
    path::PathBuf,
    process::{Command, Stdio},
    sync::mpsc,
    time::Duration,
};

/// Variables read from the login shell.
pub const LOGIN_SHELL_ENV_NAMES: [&str; 18] = [
    "PATH",
    "DBUS_SESSION_BUS_ADDRESS",
    "DISPLAY",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "SSH_AUTH_SOCK",
    "HOMEBREW_PREFIX",
    "HOMEBREW_CELLAR",
    "HOMEBREW_REPOSITORY",
    "XDG_CONFIG_HOME",
    "XDG_CURRENT_DESKTOP",
    "XDG_DATA_HOME",
    "XDG_RUNTIME_DIR",
    "XDG_SESSION_DESKTOP",
    "XDG_SESSION_TYPE",
    "WAYLAND_DISPLAY",
    "T3CODE_TELEMETRY_ENABLED",
];
const LOCALE_ENV_NAMES: [&str; 3] = ["LANG", "LC_ALL", "LC_CTYPE"];
const FALLBACK_LC_CTYPE: &str = "en_US.UTF-8";
/// The shell's own answer to these wins over what the process inherited.
const SHELL_PREFERRED: [&str; 4] = ["DBUS_SESSION_BUS_ADDRESS", "XDG_CURRENT_DESKTOP", "XDG_SESSION_DESKTOP", "XDG_SESSION_TYPE"];
/// Set from the shell only where the process has none. The telemetry opt-out is documented as a shell variable that a GUI launch never sees.
const SHELL_FILLED: [&str; 8] = ["DISPLAY", "HOMEBREW_PREFIX", "HOMEBREW_CELLAR", "HOMEBREW_REPOSITORY", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_RUNTIME_DIR", "WAYLAND_DISPLAY"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Macos,
    Linux,
    Other,
}
impl Platform {
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Macos
        } else if cfg!(target_os = "linux") {
            Self::Linux
        } else {
            Self::Other
        }
    }
}

/// What to ask, of whom, and how long to wait. `for_process` is the real configuration; tests put
/// their own shells at temporary paths and shorten the wait.
#[derive(Clone, Debug)]
pub struct Config {
    pub platform: Platform,
    /// The environment the app itself was started with.
    pub env: BTreeMap<String, String>,
    /// The shell tried after `$SHELL`; None leaves only `$SHELL`.
    pub fallback_shell: Option<String>,
    pub launchctl: PathBuf,
    pub login_shell_timeout: Duration,
    pub launchctl_timeout: Duration,
    /// How long a probe that ignored the terminate signal gets before it is killed.
    pub terminate_grace: Duration,
    /// The user id, for the Linux session bus lookup.
    pub uid: Option<u32>,
    /// Whether a path exists, for the Linux session bus lookup.
    pub exists: fn(&std::path::Path) -> bool,
}

impl Config {
    pub fn for_process() -> Self {
        let platform = Platform::current();
        Self {
            platform,
            env: std::env::vars().collect(),
            fallback_shell: match platform {
                Platform::Macos => Some("/bin/zsh".into()),
                Platform::Linux => Some("/bin/bash".into()),
                Platform::Other => None,
            },
            launchctl: PathBuf::from("/bin/launchctl"),
            login_shell_timeout: Duration::from_secs(5),
            launchctl_timeout: Duration::from_secs(2),
            terminate_grace: Duration::from_secs(1),
            #[cfg(unix)]
            uid: Some(unsafe { libc::getuid() }),
            #[cfg(not(unix))]
            uid: None,
            exists: std::path::Path::exists,
        }
    }
}

fn trim_non_empty(value: Option<&str>) -> Option<String> {
    value.map(str::trim).filter(|value| !value.is_empty()).map(str::to_owned)
}

/// The entries of each path in order, trimmed, without repeats and without empty entries.
pub fn merge_paths(values: &[Option<String>]) -> Option<String> {
    let mut entries: Vec<String> = Vec::new();
    for value in values.iter().flatten() {
        for entry in value.split(':') {
            let entry = entry.trim();
            if !entry.is_empty() && !entries.iter().any(|seen| seen == entry) {
                entries.push(entry.to_owned());
            }
        }
    }
    (!entries.is_empty()).then(|| entries.join(":"))
}

fn start_marker(name: &str) -> String {
    format!("__T3CODE_ENV_{name}_START__")
}
fn end_marker(name: &str) -> String {
    format!("__T3CODE_ENV_{name}_END__")
}

/// The one command line given to `-c`: each variable between its markers (`printenv` prints
/// nothing for an unset one; its failure does not end the line).
pub fn capture_command(names: &[&str]) -> String {
    names
        .iter()
        .map(|name| format!("printf '%s\\n' '{}'; printenv {name} || true; printf '%s\\n' '{}'", start_marker(name), end_marker(name)))
        .collect::<Vec<_>>()
        .join("; ")
}

/// What a probe printed, by name. Anything outside the markers (a profile's greeting) is ignored,
/// and a variable that is empty or whose markers are incomplete is absent.
pub fn extract_environment(output: &str, names: &[&str]) -> BTreeMap<String, String> {
    let mut found = BTreeMap::new();
    for name in names {
        let (start, end) = (start_marker(name), end_marker(name));
        let Some(begin) = output.find(&start) else { continue };
        let value_start = begin + start.len();
        let Some(finish) = output[value_start..].find(&end) else { continue };
        let value = &output[value_start..value_start + finish];
        let value = value.strip_prefix("\r\n").or_else(|| value.strip_prefix('\n')).unwrap_or(value);
        let value = value.strip_suffix("\r\n").or_else(|| value.strip_suffix('\n')).unwrap_or(value);
        if !value.is_empty() {
            found.insert((*name).to_owned(), value.to_owned());
        }
    }
    found
}

/// Runs a probe with a limit: standard input closed, standard error discarded, a terminate signal at the
/// limit and a kill after the grace. Everything that goes wrong (no such program, a failure, the limit)
/// is a line on standard error of this app and an empty answer.
fn run_probe(command: &str, args: &[String], timeout: Duration, grace: Duration, what: &str) -> String {
    let mut child = match Command::new(command).args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn() {
        Ok(child) => child,
        Err(error) => {
            eprintln!("Desktop shell environment {what} probe ({command}) failed: {error}");
            return String::new();
        }
    };
    let Some(mut stdout) = child.stdout.take() else { return String::new() };
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        let _ = sender.send(bytes);
    });
    let bytes = match receiver.recv_timeout(timeout) {
        Ok(bytes) => bytes,
        Err(_) => {
            eprintln!("Desktop shell environment {what} probe ({command}) timed out after {}ms.", timeout.as_millis());
            #[cfg(unix)]
            unsafe {
                libc::kill(child.id() as i32, libc::SIGTERM);
            }
            if receiver.recv_timeout(grace).is_err() {
                let _ = child.kill();
            }
            let _ = child.wait();
            return String::new();
        }
    };
    let _ = child.wait();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// `$SHELL`, then the platform's fallback, without repeats.
fn login_shell_candidates(config: &Config) -> Vec<String> {
    let mut candidates: Vec<String> = Vec::new();
    for candidate in [trim_non_empty(config.env.get("SHELL").map(String::as_str)), trim_non_empty(config.fallback_shell.as_deref())].into_iter().flatten() {
        if !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    }
    candidates
}

fn read_login_shell(config: &Config, shell: &str, names: &[&str]) -> BTreeMap<String, String> {
    let args = ["-ilc".to_owned(), capture_command(names)];
    extract_environment(&run_probe(shell, &args, config.login_shell_timeout, config.terminate_grace, "login-shell"), names)
}

/// The environment to start the server with: what the app has, with the login shell's answers
/// applied as the original applies them to its own process. Returned as the variables that differ,
/// to set on the server's command.
pub fn resolve(config: &Config) -> BTreeMap<String, String> {
    let mut env = config.env.clone();
    if config.platform == Platform::Other {
        return BTreeMap::new();
    }
    let mut shell_environment: BTreeMap<String, String> = BTreeMap::new();
    for shell in login_shell_candidates(config) {
        shell_environment.extend(read_login_shell(config, &shell, &LOGIN_SHELL_ENV_NAMES));
        if shell_environment.contains_key("PATH") {
            break;
        }
    }
    let launchctl_path = if config.platform == Platform::Macos && !shell_environment.contains_key("PATH") {
        trim_non_empty(Some(&run_probe(&config.launchctl.to_string_lossy(), &["getenv".into(), "PATH".into()], config.launchctl_timeout, config.terminate_grace, "launchctl-path")))
    } else {
        None
    };
    let existing_path = trim_non_empty(env.get("PATH").map(String::as_str));
    if let Some(merged) = merge_paths(&[trim_non_empty(shell_environment.get("PATH").map(String::as_str)).or(launchctl_path), existing_path]) {
        env.insert("PATH".into(), merged);
    }
    let unset = |env: &BTreeMap<String, String>, name: &str| env.get(name).is_none_or(|value| value.is_empty());
    if unset(&env, "SSH_AUTH_SOCK") {
        if let Some(value) = shell_environment.get("SSH_AUTH_SOCK") {
            env.insert("SSH_AUTH_SOCK".into(), value.clone());
        }
    }
    for name in SHELL_PREFERRED {
        if let Some(value) = shell_environment.get(name) {
            env.insert(name.into(), value.clone());
        }
    }
    for name in SHELL_FILLED.iter().chain(["T3CODE_TELEMETRY_ENABLED"].iter()) {
        if unset(&env, name) {
            if let Some(value) = shell_environment.get(*name) {
                env.insert((*name).into(), value.clone());
            }
        }
    }
    // The locale variables form one precedence group (LC_ALL overrides an inherited LANG), so the group is
    // taken from the shell only when the process has none of it. A GUI launch inherits no locale from launchd,
    // so spawned agents would run in the C locale; only LC_CTYPE is set to a UTF-8 locale then, which every
    // supported macOS has, and LANG stays unset to keep C-stable collation.
    if config.platform == Platform::Macos && LOCALE_ENV_NAMES.iter().all(|name| trim_non_empty(env.get(*name).map(String::as_str)).is_none()) {
        for name in LOCALE_ENV_NAMES {
            if let Some(value) = trim_non_empty(shell_environment.get(name).map(String::as_str)) {
                env.insert(name.into(), value);
            }
        }
        if LOCALE_ENV_NAMES.iter().all(|name| trim_non_empty(env.get(*name).map(String::as_str)).is_none()) {
            env.insert("LC_CTYPE".into(), FALLBACK_LC_CTYPE.into());
        }
    }
    if config.platform == Platform::Linux && trim_non_empty(env.get("DBUS_SESSION_BUS_ADDRESS").map(String::as_str)).is_none() {
        let mut directories = Vec::new();
        if let Some(runtime) = trim_non_empty(env.get("XDG_RUNTIME_DIR").map(String::as_str)) {
            directories.push(runtime.trim_end_matches('/').to_owned());
        }
        if let Some(uid) = config.uid {
            directories.push(format!("/run/user/{uid}"));
        }
        for directory in directories.into_iter().filter(|directory| !directory.is_empty()) {
            let bus = format!("{directory}/bus");
            if (config.exists)(std::path::Path::new(&bus)) {
                env.insert("DBUS_SESSION_BUS_ADDRESS".into(), format!("unix:path={bus}"));
                break;
            }
        }
    }
    env.into_iter().filter(|(name, value)| config.env.get(name) != Some(value)).collect()
}

/// The answer, asked for once: the first caller pays for the shell, every later start of the server
/// reuses it.
pub struct Cached {
    config: Config,
    value: std::sync::OnceLock<BTreeMap<String, String>>,
}
impl std::fmt::Debug for Cached {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Cached(..)")
    }
}
impl Cached {
    pub fn new(config: Config) -> Self {
        Self { config, value: std::sync::OnceLock::new() }
    }
    pub fn get(&self) -> &BTreeMap<String, String> {
        self.value.get_or_init(|| resolve(&self.config))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::path::Path;

    /// A shell at a temporary path that records each call and then answers as a login shell with `export_line` would:
    /// it runs the captured command in a `sh` whose environment is the fake's own, so the markers are real.
    fn shell(directory: &Path, name: &str, setup: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let path = directory.join(name);
        let log = directory.join(format!("{name}.calls"));
        std::fs::write(&path, format!("#!/bin/sh\n[ -n \"$T3_FAKE_CLEAN\" ] || exec /usr/bin/env -i T3_FAKE_CLEAN=1 /bin/sh \"$0\" \"$@\"\necho \"$1\" >> '{}'\n{setup}\nshift\nexec /bin/sh -c \"$1\"\n", log.display())).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_string_lossy().into_owned()
    }
    fn calls(directory: &Path, name: &str) -> usize {
        std::fs::read_to_string(directory.join(format!("{name}.calls"))).map_or(0, |text| text.lines().count())
    }
    fn config(directory: &Path, platform: Platform, env: &[(&str, &str)], fallback: Option<String>) -> Config {
        Config {
            platform,
            env: env.iter().map(|(key, value)| ((*key).to_owned(), (*value).to_owned())).collect(),
            fallback_shell: fallback,
            launchctl: directory.join("no-launchctl"),
            login_shell_timeout: Duration::from_secs(10),
            launchctl_timeout: Duration::from_secs(10),
            terminate_grace: Duration::from_millis(200),
            uid: Some(501),
            exists: |_| false,
        }
    }

    #[test]
    fn the_shells_path_comes_first_and_the_apps_own_follows_without_repeats() {
        let directory = tempfile::tempdir().unwrap();
        let fake = shell(directory.path(), "fake-zsh", "PATH='/opt/fake/bin:/usr/bin:/bin'; export PATH");
        let patch = resolve(&config(directory.path(), Platform::Macos, &[("SHELL", &fake), ("PATH", "/usr/bin:/bin:/launchd/only")], None));
        assert_eq!(patch["PATH"], "/opt/fake/bin:/usr/bin:/bin:/launchd/only");
        assert_eq!(calls(directory.path(), "fake-zsh"), 1, "one shell run answers every name");
        let asked = std::fs::read_to_string(directory.path().join("fake-zsh.calls")).unwrap();
        assert!(asked.contains("-ilc") || asked.contains("__T3CODE_ENV_PATH_START__"), "{asked}");
    }

    #[test]
    fn a_shell_with_no_path_gives_way_to_the_next_and_a_shell_with_one_ends_the_search() {
        let directory = tempfile::tempdir().unwrap();
        let silent = shell(directory.path(), "silent", "unset PATH; PATH=''; export PATH");
        let user = shell(directory.path(), "fallback", "PATH='/from/fallback:/usr/bin:/bin'; export PATH");
        let never = shell(directory.path(), "never", "PATH='/never:/usr/bin'; export PATH");
        let patch = resolve(&config(directory.path(), Platform::Linux, &[("SHELL", &silent), ("PATH", "/usr/bin")], Some(user.clone())));
        assert_eq!(patch["PATH"], "/from/fallback:/usr/bin:/bin");
        assert_eq!((calls(directory.path(), "silent"), calls(directory.path(), "fallback")), (1, 1));
        // The same shell named twice is asked once, and one that answered is the last asked.
        let patch = resolve(&config(directory.path(), Platform::Linux, &[("SHELL", &user)], Some(user.clone())));
        assert_eq!(patch["PATH"], "/from/fallback:/usr/bin:/bin");
        assert_eq!(calls(directory.path(), "fallback"), 2);
        let _ = never;
        assert_eq!(calls(directory.path(), "never"), 0);
    }

    #[test]
    fn a_shell_that_hangs_is_ended_at_the_limit_and_the_next_is_tried() {
        let directory = tempfile::tempdir().unwrap();
        // `exec sleep` so that the terminate signal ends it; a shell that ignored the signal would be killed after the grace.
        let hung = {
            use std::os::unix::fs::PermissionsExt;
            let path = directory.path().join("hung");
            std::fs::write(&path, "#!/bin/sh\nexec sleep 30\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path.to_string_lossy().into_owned()
        };
        let stubborn = {
            use std::os::unix::fs::PermissionsExt;
            let path = directory.path().join("stubborn");
            std::fs::write(&path, "#!/bin/sh\ntrap '' TERM\nwhile :; do sleep 1; done\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path.to_string_lossy().into_owned()
        };
        let good = shell(directory.path(), "good", "PATH='/after/hang:/usr/bin:/bin'; export PATH");
        for slow in [hung, stubborn] {
            let mut config = config(directory.path(), Platform::Linux, &[("SHELL", &slow), ("PATH", "/usr/bin")], Some(good.clone()));
            // The limit is also the shell that answers' (`good`): a limit short enough for a loaded machine to overrun with a plain `sh`
            // made this test fail whenever builds ran beside it (the probe of `good` timed out as the hung one did).
            config.login_shell_timeout = Duration::from_millis(2000);
            let started = std::time::Instant::now();
            let patch = resolve(&config);
            assert_eq!(patch["PATH"], "/after/hang:/usr/bin:/bin", "{slow}");
            assert!(started.elapsed() < Duration::from_secs(8), "the probe was ended, not waited for: {:?}", started.elapsed());
        }
    }

    #[test]
    fn a_profile_that_talks_does_not_disturb_the_answer_and_an_unset_variable_is_absent() {
        let output = "welcome to my shell\n__T3CODE_ENV_PATH_START__\n/a:/b\n__T3CODE_ENV_PATH_END__\nnoise\n__T3CODE_ENV_LANG_START__\n__T3CODE_ENV_LANG_END__\n__T3CODE_ENV_DISPLAY_START__\n:0\n";
        let found = extract_environment(output, &["PATH", "LANG", "DISPLAY", "SSH_AUTH_SOCK"]);
        assert_eq!(found.get("PATH").map(String::as_str), Some("/a:/b"));
        assert!(!found.contains_key("LANG"), "empty is absent");
        assert!(!found.contains_key("DISPLAY"), "an end marker that never came is absent");
        assert!(!found.contains_key("SSH_AUTH_SOCK"));
        assert_eq!(extract_environment("__T3CODE_ENV_PATH_START__\r\n/a\r\n__T3CODE_ENV_PATH_END__", &["PATH"])["PATH"], "/a");
        assert_eq!(merge_paths(&[Some(" /a : /b::/a ".into()), None, Some("/c:/b".into())]).as_deref(), Some("/a:/b:/c"));
        assert_eq!(merge_paths(&[None, Some(" : ".into())]), None);
    }

    #[test]
    fn macos_asks_launchctl_only_when_no_shell_supplied_a_path() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let launchctl = directory.path().join("launchctl");
        std::fs::write(&launchctl, "#!/bin/sh\nprintf '/usr/local/bin:/usr/bin\\n'\n").unwrap();
        std::fs::set_permissions(&launchctl, std::fs::Permissions::from_mode(0o755)).unwrap();
        let silent = shell(directory.path(), "silent", "PATH=''; export PATH");
        let mut quiet = config(directory.path(), Platform::Macos, &[("SHELL", &silent), ("PATH", "/usr/bin:/bin")], None);
        quiet.launchctl = launchctl.clone();
        assert_eq!(resolve(&quiet)["PATH"], "/usr/local/bin:/usr/bin:/bin");
        let good = shell(directory.path(), "good", "PATH='/shell/bin:/usr/bin:/bin'; export PATH");
        let mut answered = config(directory.path(), Platform::Macos, &[("SHELL", &good), ("PATH", "/usr/bin")], None);
        answered.launchctl = launchctl.clone();
        assert_eq!(resolve(&answered)["PATH"], "/shell/bin:/usr/bin:/bin");
        // On Linux it is never asked.
        let mut linux = config(directory.path(), Platform::Linux, &[("SHELL", &silent), ("PATH", "/usr/bin")], None);
        linux.launchctl = launchctl;
        assert!(!resolve(&linux).contains_key("PATH"), "nothing to add: {:?}", resolve(&linux));
    }

    #[test]
    fn variables_follow_the_originals_precedence_and_the_locale_is_one_group() {
        let directory = tempfile::tempdir().unwrap();
        let setup = "PATH='/p:/usr/bin:/bin'; XDG_SESSION_TYPE=wayland; SSH_AUTH_SOCK=/shell/agent; HOMEBREW_PREFIX=/opt/homebrew; XDG_DATA_HOME=/shell/data; T3CODE_TELEMETRY_ENABLED=0; LC_CTYPE=en_GB.UTF-8; export PATH XDG_SESSION_TYPE SSH_AUTH_SOCK HOMEBREW_PREFIX XDG_DATA_HOME T3CODE_TELEMETRY_ENABLED LC_CTYPE";
        let fake = shell(directory.path(), "rich", setup);
        let patch = resolve(&config(directory.path(), Platform::Macos, &[("SHELL", &fake), ("PATH", "/usr/bin"), ("XDG_SESSION_TYPE", "x11"), ("SSH_AUTH_SOCK", "/launchd/agent"), ("XDG_DATA_HOME", "/app/data")], None));
        assert_eq!(patch["XDG_SESSION_TYPE"], "wayland", "the shell's word wins for the session variables");
        assert!(!patch.contains_key("SSH_AUTH_SOCK"), "an inherited agent socket is kept");
        assert!(!patch.contains_key("XDG_DATA_HOME"), "an inherited value is kept");
        assert_eq!(patch["HOMEBREW_PREFIX"], "/opt/homebrew");
        assert_eq!(patch["T3CODE_TELEMETRY_ENABLED"], "0");
        assert_eq!(patch["LC_CTYPE"], "en_GB.UTF-8", "no locale inherited: the shell's is taken");
        // With any of the group inherited, none of it is touched.
        let inherited = resolve(&config(directory.path(), Platform::Macos, &[("SHELL", &fake), ("LANG", "fr_FR.UTF-8")], None));
        assert!(!inherited.contains_key("LC_CTYPE") && !inherited.contains_key("LANG"));
        // No locale anywhere: only LC_CTYPE, and only on macOS.
        let plain = shell(directory.path(), "plain", "PATH='/p:/usr/bin:/bin'; export PATH");
        let patch = resolve(&config(directory.path(), Platform::Macos, &[("SHELL", &plain)], None));
        assert_eq!((patch["LC_CTYPE"].as_str(), patch.contains_key("LANG")), ("en_US.UTF-8", false));
        assert!(!resolve(&config(directory.path(), Platform::Linux, &[("SHELL", &plain)], None)).contains_key("LC_CTYPE"));
    }

    #[test]
    fn linux_finds_the_session_bus_where_the_runtime_directory_says_and_then_under_the_user_id() {
        let directory = tempfile::tempdir().unwrap();
        let plain = shell(directory.path(), "plain", "PATH='/p:/usr/bin:/bin'; export PATH");
        let mut found = config(directory.path(), Platform::Linux, &[("SHELL", &plain), ("XDG_RUNTIME_DIR", "/run/custom/")], None);
        found.exists = |path| path == Path::new("/run/user/501/bus");
        assert_eq!(resolve(&found)["DBUS_SESSION_BUS_ADDRESS"], "unix:path=/run/user/501/bus");
        found.exists = |path| path == Path::new("/run/custom/bus");
        assert_eq!(resolve(&found)["DBUS_SESSION_BUS_ADDRESS"], "unix:path=/run/custom/bus");
        found.exists = |_| false;
        assert!(!resolve(&found).contains_key("DBUS_SESSION_BUS_ADDRESS"));
    }

    #[test]
    fn the_answer_is_asked_for_once_per_run_and_other_platforms_change_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let fake = shell(directory.path(), "once", "PATH='/once:/usr/bin:/bin'; export PATH");
        let cached = Cached::new(config(directory.path(), Platform::Linux, &[("SHELL", &fake)], None));
        assert_eq!(cached.get()["PATH"], "/once:/usr/bin:/bin");
        assert_eq!(cached.get()["PATH"], "/once:/usr/bin:/bin");
        assert_eq!(calls(directory.path(), "once"), 1);
        assert!(resolve(&config(directory.path(), Platform::Other, &[("SHELL", &fake)], None)).is_empty());
        assert_eq!(calls(directory.path(), "once"), 1);
    }
}
