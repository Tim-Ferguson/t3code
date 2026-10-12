//! Host setup the shared headless browser needs, and the one command that does it (apps/server/src/preview/PreviewBrowserHost.ts). Every
//! failure here names that command, so the server log, an agent's tool error, and the viewer all offer the same fix: `sudo t3 browser setup`.
//!
//! T3 never turns Chrome's sandbox off by itself. A host that cannot give it one gets the command instead, and only the operator's explicit
//! `T3CODE_SERVER_BROWSER_SANDBOX=0` launches without it.
use std::{future::Future, path::Path, time::Duration};

/// The subcommand that sets the host up; callers render it, with `sudo`, for how T3 was launched.
pub const SETUP_SUBCOMMAND: &str = "browser setup";

/// What Chrome prints before aborting when it cannot sandbox itself.
pub const NO_SANDBOX_SIGNATURE: &str = "No usable sandbox";

/// Whether an operator explicitly allowed Chrome to run without its sandbox.
pub fn sandbox_disabled(variable: impl Fn(&str) -> Option<String>) -> bool {
    variable("T3CODE_SERVER_BROWSER_SANDBOX").as_deref() == Some("0")
}

/// Set on Ubuntu 23.10+: unprivileged user namespaces need an AppArmor profile.
const USERNS_RESTRICTION: &str = "/proc/sys/kernel/apparmor_restrict_unprivileged_userns";
pub const APPARMOR_PROFILE_PATH: &str = "/etc/apparmor.d/t3-chrome-headless-shell";

/// Lets T3's headless browser, in any T3 home and at any pinned version, create the user namespace Chrome's sandbox runs in. Modelled on the
/// profile Ubuntu ships for Google Chrome; `unconfined` adds nothing beyond `userns`.
pub const APPARMOR_PROFILE: &str = "# Written by `t3 browser setup`: lets T3 Code's headless browser use Chrome's sandbox.
abi <abi/4.0>,
include <tunables/global>

profile t3-chrome-headless-shell /**/tools/chrome-headless-shell/*/*/chrome-headless-shell flags=(unconfined) {
  userns,

  include if exists <local/t3-chrome-headless-shell>
}
";

/// Chrome's Debian dependencies that minimal images leave out, from the headless shell's deb.deps. Ubuntu 24.04 and Debian 13 renamed some
/// with a `t64` suffix; setup installs whichever name the host's apt offers.
pub const DEBIAN_PACKAGES: &[&[&str]] = &[
    &["libnss3"],
    &["libglib2.0-0t64", "libglib2.0-0"],
    &["libatk1.0-0t64", "libatk1.0-0"],
    &["libatk-bridge2.0-0t64", "libatk-bridge2.0-0"],
    &["libatspi2.0-0t64", "libatspi2.0-0"],
    &["libdbus-1-3"],
    &["libx11-6"],
    &["libxcb1"],
    &["libxcomposite1"],
    &["libxdamage1"],
    &["libxext6"],
    &["libxfixes3"],
    &["libxrandr2"],
    &["libxkbcommon0"],
    &["libgbm1"],
    &["libasound2t64", "libasound2"],
    &["libexpat1"],
];

/// A host setup the browser is missing, which the setup command fixes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewBrowserHostError {
    Sandbox { setup_command: String },
    Libraries { setup_command: String, libraries: Vec<String> },
}

impl PreviewBrowserHostError {
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Sandbox { .. } => "PreviewBrowserSandboxError",
            Self::Libraries { .. } => "PreviewBrowserLibrariesError",
        }
    }
    pub fn message(&self) -> String {
        match self {
            Self::Sandbox { setup_command } => format!("This host blocks the sandbox T3's browser runs in (AppArmor on Ubuntu 23.10+). Run `{setup_command}` on the host once to allow it, then try again."),
            Self::Libraries { setup_command, libraries } => format!("This host is missing libraries T3's browser needs ({}). Run `{setup_command}` on the host to install them, then try again.", libraries.join(", ")),
        }
    }
}
impl std::fmt::Display for PreviewBrowserHostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}
impl std::error::Error for PreviewBrowserHostError {}

/// The libraries an `ldd` report says the loader cannot find: lines of the form `<name> => not found`.
pub fn missing_libraries_in(report: &str) -> Vec<String> {
    report
        .lines()
        .filter_map(|line| line.trim_start().strip_suffix(" => not found").filter(|name| !name.is_empty() && !name.contains(char::is_whitespace)).map(str::to_owned))
        .collect()
}

/// Shared libraries the loader cannot find for `executable`, by `ldd`. Nothing when `ldd` is not there or takes more than five seconds.
pub async fn missing_libraries(executable: &Path) -> Vec<String> {
    let run = tokio::process::Command::new("ldd").arg(executable).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null()).kill_on_drop(true).output();
    match tokio::time::timeout(Duration::from_secs(5), run).await {
        Ok(Ok(output)) => missing_libraries_in(&String::from_utf8_lossy(&output.stdout)),
        _ => Vec::new(),
    }
}

/// After a launch fails, names the host setup it is missing: the sandbox, from Chrome's own abort message, or shared libraries, from `ldd`.
/// None when neither explains it. Linux only; other hosts never need either.
pub async fn diagnose_launch_failure(executable: &Path, output: &str, setup_command: &str) -> Option<PreviewBrowserHostError> {
    diagnose(std::env::consts::OS, output, setup_command, || missing_libraries(executable)).await
}

/// `diagnose_launch_failure` with the platform given and the library check supplied.
pub async fn diagnose<F, Fut>(platform: &str, output: &str, setup_command: &str, libraries: F) -> Option<PreviewBrowserHostError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Vec<String>>,
{
    let setup_command = setup_command.to_owned();
    if output.contains(NO_SANDBOX_SIGNATURE) {
        return Some(PreviewBrowserHostError::Sandbox { setup_command });
    }
    if platform != "linux" {
        return None;
    }
    let libraries = libraries().await;
    (!libraries.is_empty()).then_some(PreviewBrowserHostError::Libraries { setup_command, libraries })
}

/// Whether Chrome's sandbox will be blocked here: the host restricts user namespaces and T3's AppArmor profile is not installed. Readable
/// without root, so the server checks it at startup.
pub async fn sandbox_blocked() -> bool {
    sandbox_blocked_on(std::env::consts::OS, Path::new(USERNS_RESTRICTION), Path::new(APPARMOR_PROFILE_PATH)).await
}

pub async fn sandbox_blocked_on(platform: &str, restriction: &Path, profile: &Path) -> bool {
    if platform != "linux" {
        return false;
    }
    let restricted = tokio::fs::read_to_string(restriction).await.is_ok_and(|value| value.trim() == "1");
    restricted && !tokio::fs::try_exists(profile).await.unwrap_or(false)
}
