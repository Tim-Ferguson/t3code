//! Updating the connected server from the client: when it is behind, what the server offers to do about it, the progress
//! of the update and what to hand a person whose server cannot update itself. Port of `apps/web/src/versionSkew.ts`, the
//! pure parts of `ServerUpdateAction.tsx` and the wire of `updateServer`/`updateServerWithProgress`/`commitDesktopUpdate`.
//! This server answers all three with every effect refused unless a desktop app or a launcher drives it (nothing is
//! downloaded, installed or relaunched by itself), so the states a refusal leaves are as real as the successful ones.
use serde_json::{Value, json};
use std::cmp::Ordering;

pub const UPDATE: &str = "server.updateServer";
pub const UPDATE_WITH_PROGRESS: &str = "server.updateServerWithProgress";
pub const COMMIT_DESKTOP_UPDATE: &str = "server.commitDesktopUpdate";
pub const DISMISSALS_KEY: &str = "t3code:version-mismatch-dismissals:v1";

/// The version of this client (`APP_VERSION`).
pub fn app_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mismatch {
    pub client_version: String,
    pub server_version: String,
}
pub const MISMATCH_HINT: &str = "Version mismatch. Try syncing the client and server to the same T3 Code version.";

#[derive(Debug, Clone, PartialEq, Eq)]
struct Semver {
    core: [u64; 3],
    prerelease: Vec<String>,
}
fn parse_semver(text: &str) -> Option<Semver> {
    let text = text.split('+').next()?;
    let (core, prerelease) = match text.split_once('-') {
        Some((core, prerelease)) => (core, prerelease.split('.').map(str::to_owned).collect()),
        None => (text, vec![]),
    };
    let mut numbers = core.split('.').map(|part| (!part.is_empty() && part.bytes().all(|b| b.is_ascii_digit())).then(|| part.parse::<u64>().ok()).flatten());
    let semver = Semver { core: [numbers.next()??, numbers.next()??, numbers.next()??], prerelease };
    numbers.next().is_none().then_some(semver)
}
fn compare_semver(a: &Semver, b: &Semver) -> Ordering {
    a.core.cmp(&b.core).then_with(|| match (a.prerelease.is_empty(), b.prerelease.is_empty()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => a
            .prerelease
            .iter()
            .zip(&b.prerelease)
            .map(|(x, y)| match (x.parse::<u64>(), y.parse::<u64>()) {
                (Ok(x), Ok(y)) => x.cmp(&y),
                (Ok(_), Err(_)) => Ordering::Less,
                (Err(_), Ok(_)) => Ordering::Greater,
                (Err(_), Err(_)) => x.cmp(y),
            })
            .find(|ordering| ordering.is_ne())
            .unwrap_or_else(|| a.prerelease.len().cmp(&b.prerelease.len())),
    })
}
fn version_core(version: &str) -> &str {
    version.split(['-', '+']).next().unwrap_or(version)
}

/// `resolveVersionMismatch`: the skew a person can act on, which is a server running an older T3 Code than this client. Two
/// nightly builds compare in full; other pairs compare their `major.minor.patch`, so a stable and a nightly build of one core
/// do not warn. A server ahead of the client needs nothing. Versions that do not parse differ by plain inequality.
pub fn resolve_version_mismatch(client: &str, server: Option<&str>) -> Option<Mismatch> {
    let (client, server) = (client.trim(), server?.trim());
    if client.is_empty() || server.is_empty() {
        return None;
    }
    let nightly = |version: &str| parse_semver(version).is_some_and(|semver| semver.prerelease.first().map(String::as_str) == Some("nightly"));
    let both_nightly = nightly(client) && nightly(server);
    let behind = match (parse_semver(version_core(client)), parse_semver(version_core(server))) {
        (Some(client_core), Some(server_core)) => {
            if both_nightly {
                match (parse_semver(client), parse_semver(server)) {
                    (Some(c), Some(s)) => compare_semver(&s, &c) == Ordering::Less,
                    _ => server != client,
                }
            } else {
                compare_semver(&server_core, &client_core) == Ordering::Less
            }
        }
        _ => server != client,
    };
    behind.then(|| Mismatch { client_version: client.to_owned(), server_version: server.to_owned() })
}

pub fn dismissal_key(environment: &str, mismatch: &Mismatch) -> String {
    format!("{environment}:{}:{}", mismatch.client_version, mismatch.server_version)
}

/// `manualServerUpdateCommand`: what to hand a person whose server cannot update itself. `installation` is the descriptor's
/// `serverInstallation`.
pub fn manual_command(target_version: &str, installation: Option<&Value>) -> String {
    let kind = installation.and_then(|installation| installation["kind"].as_str());
    if kind == Some("npm-global") {
        let prefix = installation.and_then(|installation| installation["prefix"].as_str()).unwrap_or_default().replace('\'', "'\\''");
        return format!("npm install --global --prefix '{prefix}' t3@{target_version}");
    }
    let runner = match kind {
        Some("pnpm-dlx") => "pnpm dlx",
        Some("bunx") => "bunx",
        _ => "npx",
    };
    format!("{runner} t3@{target_version}")
}
/// What the copy of that command tells the person to do with it.
pub fn manual_copy_label(installation: Option<&Value>) -> &'static str {
    if installation.is_some_and(|installation| installation["kind"] == "npm-global") { "Copy update command" } else { "Copy relaunch command" }
}
pub fn manual_copied(command: &str, server_label: &str, installation: Option<&Value>) -> (String, String) {
    if installation.is_some_and(|installation| installation["kind"] == "npm-global") {
        ("Update command copied".into(), format!("Run `{command}` on {server_label}, then restart t3 with your usual options."))
    } else {
        ("Relaunch command copied".into(), format!("Stop t3 on {server_label}, then relaunch with `{command}` using the same subcommand and options. This does not update an installed t3 command."))
    }
}

/// `serverUpdateGuidance`.
pub fn guidance(capability: &str) -> &'static str {
    if capability == "desktop-managed" { "Update the desktop app" } else { "Update to stay in sync" }
}

/// What the descriptor offers (`capabilities.serverSelfUpdate`, `desktopAppUpdate`, ...), as the update flow reads it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Offer {
    /// `boot-service`, `respawn` or `desktop-managed`; none when the server can only be relaunched by hand.
    pub self_update: Option<String>,
    pub desktop_app_update: bool,
    pub progress: bool,
    pub thread_continuation: bool,
    pub installation: Option<Value>,
}
impl Offer {
    pub fn of(capabilities: &Value) -> Self {
        Self {
            self_update: capabilities["serverSelfUpdate"].as_str().map(str::to_owned),
            desktop_app_update: capabilities["desktopAppUpdate"] == true,
            progress: capabilities["serverSelfUpdateProgress"] == true,
            thread_continuation: capabilities["serverUpdateThreadContinuation"] == true,
            installation: capabilities.get("serverInstallation").filter(|installation| installation.is_object()).cloned(),
        }
    }
    /// Whether this client can have the server update itself: not when only the desktop app that runs it could, and that
    /// app does not take the request.
    pub fn remote_update(&self) -> bool {
        match self.self_update.as_deref() {
            None => false,
            Some("desktop-managed") => self.desktop_app_update,
            Some(_) => true,
        }
    }
    pub fn desktop_managed(&self) -> bool {
        self.self_update.as_deref() == Some("desktop-managed")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Downloading,
    Installing,
    Resuming,
}
impl Stage {
    /// The wire's "installing" is a sub-second launcher handoff, so the label folds it into the download; what follows is the
    /// restart the person is waiting through.
    pub fn label(self) -> &'static str {
        match self {
            Self::Downloading | Self::Installing => "Downloading\u{2026}",
            Self::Resuming => "Restarting\u{2026}",
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum State {
    #[default]
    Idle,
    Running { stage: Stage, target_version: String },
    Failed { message: String, target_version: String },
}

/// `ServerSelfUpdateInput`: the version to move to, and whether turns running now are to be recovered after the restart.
pub fn input(target_version: &str, continue_running_threads: bool) -> Value {
    if continue_running_threads {
        json!({"targetVersion": target_version, "continueRunningThreads": true})
    } else {
        json!({"targetVersion": target_version})
    }
}

/// One item of `updateServerWithProgress`.
#[derive(Debug, Clone, PartialEq)]
pub enum Progress {
    Stage(Stage),
    Complete(Value),
}
pub fn parse_progress(item: &Value) -> Option<Progress> {
    match item["type"].as_str()? {
        "progress" => Some(Progress::Stage(match item["stage"].as_str()? {
            "downloading" => Stage::Downloading,
            "installing" => Stage::Installing,
            _ => return None,
        })),
        "complete" => Some(Progress::Complete(item["result"].clone())),
        _ => None,
    }
}

/// The notice a finished update leaves.
pub fn finished(server_label: &str, desktop_managed: bool, result: &Value) -> (String, String) {
    let version = result["targetVersion"].as_str().unwrap_or_default();
    (format!("{server_label} updated"), if desktop_managed { format!("Desktop app relaunched on {version}.") } else { format!("Reconnected on t3@{version}.") })
}
/// The token the desktop app prepared the update under, to be committed once the person has confirmed.
pub fn desktop_token(result: &Value) -> Option<&str> {
    (result["method"] == "desktop-app").then(|| result["desktopUpdateToken"].as_str()).flatten()
}

/// The question asked before a desktop app is told to update itself, which closes and relaunches it.
pub fn confirm_desktop_message(server_label: &str) -> String {
    format!("Update the T3 Code desktop app that runs the {server_label}? It will close and relaunch on that machine.")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn behind(client: &str, server: &str) -> bool {
        resolve_version_mismatch(client, Some(server)).is_some()
    }

    #[test]
    fn only_a_server_behind_its_client_is_a_mismatch() {
        assert!(behind("1.2.0", "1.1.9"));
        assert!(!behind("1.2.0", "1.2.0"));
        assert!(!behind("1.2.0", "1.3.0"), "a server ahead needs nothing");
        // A stable and a nightly build of one core do not warn; core versions decide.
        assert!(!behind("1.2.0", "1.2.0-nightly.20260101.1"));
        assert!(!behind("1.2.0-nightly.20260101.1", "1.2.0"));
        // Two nightlies compare in full.
        assert!(behind("1.2.0-nightly.20260102.1", "1.2.0-nightly.20260101.5"));
        assert!(!behind("1.2.0-nightly.20260101.1", "1.2.0-nightly.20260101.1"));
        assert!(!behind("1.2.0-nightly.20260101.5", "1.2.0-nightly.20260102.1"));
        // What does not parse differs by inequality, and a missing version is no mismatch.
        assert!(behind("1.2.0", "rust-test"));
        assert!(!behind("custom", "custom"));
        assert_eq!(resolve_version_mismatch("1.2.0", None), None);
        assert_eq!(resolve_version_mismatch("1.2.0", Some("  ")), None);
        let mismatch = resolve_version_mismatch("1.2.0", Some("1.1.0")).unwrap();
        assert_eq!(dismissal_key("env", &mismatch), "env:1.2.0:1.1.0");
    }

    #[test]
    fn the_manual_command_follows_how_the_server_was_installed() {
        assert_eq!(manual_command("1.2.0", None), "npx t3@1.2.0");
        assert_eq!(manual_command("1.2.0", Some(&json!({"kind": "pnpm-dlx"}))), "pnpm dlx t3@1.2.0");
        assert_eq!(manual_command("1.2.0", Some(&json!({"kind": "bunx"}))), "bunx t3@1.2.0");
        assert_eq!(manual_command("1.2.0", Some(&json!({"kind": "npm-global", "prefix": "/opt/it's"}))), "npm install --global --prefix '/opt/it'\\''s' t3@1.2.0");
        let (title, _) = manual_copied("x", "Box", Some(&json!({"kind": "npm-global", "prefix": "/p"})));
        assert_eq!((title.as_str(), manual_copy_label(None)), ("Update command copied", "Copy relaunch command"));
    }

    #[test]
    fn what_the_server_offers_decides_who_can_update_it() {
        let offer = |caps: Value| Offer::of(&caps);
        assert!(!offer(json!({})).remote_update());
        assert!(offer(json!({"serverSelfUpdate": "boot-service"})).remote_update());
        assert!(!offer(json!({"serverSelfUpdate": "desktop-managed"})).remote_update(), "the app that runs it must take the request");
        assert!(offer(json!({"serverSelfUpdate": "desktop-managed", "desktopAppUpdate": true})).remote_update());
        assert_eq!(guidance("desktop-managed"), "Update the desktop app");
        assert_eq!(guidance("respawn"), "Update to stay in sync");
    }

    #[test]
    fn progress_items_become_stages_and_the_result_the_desktop_token() {
        assert_eq!(parse_progress(&json!({"type": "progress", "stage": "installing"})), Some(Progress::Stage(Stage::Installing)));
        assert_eq!(Stage::Installing.label(), Stage::Downloading.label());
        assert_eq!(Stage::Resuming.label(), "Restarting\u{2026}");
        let result = json!({"targetVersion": "1.2.0", "method": "desktop-app", "desktopUpdateToken": "tok"});
        assert_eq!(parse_progress(&json!({"type": "complete", "result": result})), Some(Progress::Complete(result.clone())));
        assert_eq!(desktop_token(&result), Some("tok"));
        assert_eq!(desktop_token(&json!({"method": "boot-service", "desktopUpdateToken": "x"})), None);
        assert_eq!(finished("Box", true, &result).1, "Desktop app relaunched on 1.2.0.");
        assert_eq!(finished("Box", false, &result).1, "Reconnected on t3@1.2.0.");
        assert_eq!(input("1.2.0", true), json!({"targetVersion": "1.2.0", "continueRunningThreads": true}));
    }
}
