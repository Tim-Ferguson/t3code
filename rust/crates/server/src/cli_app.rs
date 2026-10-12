//! `t3 app [path] [--base-dir DIR]` (`cli/app.ts`): opens a project in the running T3 Code desktop app on this machine.
//!
//! The command asks over the desktop's control socket (`t3_remote::desktop_app_control`); a running server is not enough, the
//! desktop app itself must be listening. It touches no state: nothing is created when no desktop app answers.
//!
//! - It refuses over SSH (`SSH_CONNECTION` or `SSH_TTY` set and not empty) and on a platform that is none of macOS, Linux and
//!   Windows, before it looks for anything.
//! - The T3 home is `--base-dir`, else `T3CODE_HOME`, else `~/.t3`; the desktop listens at the address of `<home>/userdata`. Only
//!   when neither the flag nor a non-blank `T3CODE_HOME` names a home is a development desktop's `<home>/dev` tried, and only when
//!   nothing is listening at the first address (a refused connection, not an answer of any kind).
//! - The project is the path given, else the current directory, with `~` expanded and made absolute.
use crate::cli_commands::{Outcome, Spec, expand_home, parse};
use std::path::{Path, PathBuf};
use t3_contracts::{DesktopAppActivationRequest, DesktopAppActivationResponse};
use t3_remote::desktop_app_control::{CLI_RESPONSE_TIMEOUT, SendError, resolve_address, send_request};

const SPEC: Spec = Spec { values: &["base-dir"], booleans: &[], repeated: &[], positionals: (0, 1) };

/// Everything about the machine `t3 app` reads, so a test names it.
pub struct Host<'a> {
    pub env: &'a dyn Fn(&str) -> Option<String>,
    /// `process.platform`.
    pub platform: &'a str,
    pub cwd: PathBuf,
    pub temp_dir: PathBuf,
    pub user_id: Option<u32>,
    pub request_id: String,
}

/// `DesktopAppSshUnsupportedError`.
pub const SSH_UNSUPPORTED: &str = "`t3 app` only controls a desktop app on the same machine. It cannot run over SSH.";
/// `DesktopAppUnreachableError`.
pub const UNREACHABLE: &str = "Could not reach the T3 Code desktop app. Start or update the desktop app on this machine, then run `t3 app` again. A running T3 Code server is not enough.";

/// `path.resolve`: joins onto the working directory unless absolute, and removes `.` and `..` segments without touching the disk.
fn resolve(path: &Path, cwd: &Path) -> PathBuf {
    use std::path::Component;
    let joined = if path.is_absolute() { path.to_path_buf() } else { cwd.join(path) };
    let mut resolved = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::ParentDir => {
                resolved.pop();
            }
            Component::CurDir => {}
            other => resolved.push(other.as_os_str()),
        }
    }
    resolved
}

async fn run(args: &[String], host: Host<'_>) -> Result<String, String> {
    let parsed = parse(args, &SPEC)?;
    let set = |name: &str| (host.env)(name).filter(|value| !value.is_empty());
    if set("SSH_CONNECTION").is_some() || set("SSH_TTY").is_some() {
        return Err(SSH_UNSUPPORTED.into());
    }
    let Some(platform) = t3_contracts::DesktopAppActivationPlatform::from_node_name(host.platform) else {
        return Err(format!("`t3 app` is not supported on {}.", host.platform));
    };

    let explicit_home = parsed.value("base-dir").map(str::to_owned).or_else(|| set("T3CODE_HOME"));
    let base_dir = match explicit_home.as_deref().map(str::trim).filter(|home| !home.is_empty()) {
        Some(home) => resolve(&expand_home(home, host.env), &host.cwd),
        None => expand_home("~", host.env).join(".t3"),
    };
    let allow_dev_fallback = parsed.value("base-dir").is_none() && set("T3CODE_HOME").is_none_or(|home| home.trim().is_empty());
    let raw_root = parsed.positionals.first().cloned().unwrap_or_else(|| host.cwd.to_string_lossy().into_owned());
    let workspace_root = resolve(&expand_home(&raw_root, host.env), &host.cwd).to_string_lossy().into_owned();
    let address_of = |state_subdirectory: &str| resolve_address(&base_dir.join(state_subdirectory), host.platform, &host.temp_dir, host.user_id).address;

    let request = DesktopAppActivationRequest::parse(&serde_json::json!({
        "version": t3_contracts::DESKTOP_APP_ACTIVATION_PROTOCOL_VERSION,
        "requestId": host.request_id,
        "type": "open-workspace",
        "workspaceRoot": workspace_root,
        "platform": platform.node_name(),
    })
    .to_string())
    .map_err(|_| "The workspace path is empty.".to_owned())?;
    let address = address_of("userdata");
    let fallback = allow_dev_fallback.then(|| address_of("dev"));
    let response = send_request(&address, fallback.as_deref(), &request, CLI_RESPONSE_TIMEOUT).await.map_err(|SendError { .. }| UNREACHABLE.to_owned())?;
    if let DesktopAppActivationResponse::Failure(failure) = &response {
        return Err(format!("T3 Code could not open {workspace_root} ({}).", failure.code.as_str()));
    }
    Ok(format!("Opened {workspace_root} in T3 Code."))
}

/// The command, run against a described host.
pub fn app_command_on(args: &[String], host: Host<'_>) -> Outcome {
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => return Outcome::failure(error.to_string()),
    };
    match runtime.block_on(run(args, host)) {
        Ok(text) => Outcome::log(text),
        Err(message) => Outcome::failure(message),
    }
}

/// `t3 app` on this machine.
pub fn app_command(args: &[String], env: &dyn Fn(&str) -> Option<String>) -> Outcome {
    let Ok(cwd) = std::env::current_dir() else { return Outcome::failure("The current directory is not available.") };
    app_command_on(
        args,
        Host {
            env,
            platform: t3_contracts::node_platform_name(),
            cwd,
            temp_dir: t3_remote::desktop_app_control::node_tmpdir(env),
            user_id: t3_remote::desktop_app_control::current_user_id(),
            request_id: uuid::Uuid::new_v4().to_string(),
        },
    )
}
