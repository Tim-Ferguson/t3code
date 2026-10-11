//! SSH password helpers (`packages/ssh/src/auth.ts`). When a key-less host asks for a password the
//! desktop app prompts in its own window and re-runs `ssh` with `SSH_ASKPASS` pointing at a tiny
//! script that echoes the cached password from `T3_SSH_AUTH_SECRET`. This module renders those
//! scripts, builds the environment for such a run, recognises authentication failures in ssh's
//! stderr, and defines the prompt service the shell implements ([`SshPasswordPrompt`]).
//!
//! The script texts are the original's (`src/ssh_scripts/askpass_*.tmpl`) and are compared byte for
//! byte with it in `tests/fixture_parity_ssh_tunnel.rs`.

use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;
use std::sync::OnceLock;

use async_trait::async_trait;
use regex::Regex;

const SSH_ASKPASS_DIR_NAME: &str = "t3code-ssh-askpass";
pub const ASKPASS_POSIX_SCRIPT: &str = include_str!("ssh_scripts/askpass_posix.tmpl");
pub const ASKPASS_WINDOWS_LAUNCHER_SCRIPT: &str =
    include_str!("ssh_scripts/askpass_windows_launcher.tmpl");
pub const ASKPASS_WINDOWS_SCRIPT: &str = include_str!("ssh_scripts/askpass_windows.tmpl");

/// `SshPasswordRequest`: what the prompt window shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshPasswordRequest {
    pub destination: String,
    pub username: Option<String>,
    pub prompt: String,
    /// 1 for the first password the user is asked for, 2 for the retry after a rejection.
    pub attempt: u32,
}

/// `SshPasswordPromptError`
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct SshPasswordPromptError {
    pub message: String,
    pub cause: Option<String>,
}

/// `SshPasswordPrompt`. The desktop shell implements it with a window; `None` from `request` is
/// the user cancelling.
#[async_trait]
pub trait SshPasswordPrompt: Send + Sync {
    fn is_available(&self) -> bool;
    async fn request(
        &self,
        request: SshPasswordRequest,
    ) -> Result<Option<String>, SshPasswordPromptError>;
}

/// `SshPasswordPrompt.disabledLayer`: headless callers; ssh then runs with its own prompts.
pub struct DisabledSshPasswordPrompt;

#[async_trait]
impl SshPasswordPrompt for DisabledSshPasswordPrompt {
    fn is_available(&self) -> bool {
        false
    }

    async fn request(
        &self,
        _request: SshPasswordRequest,
    ) -> Result<Option<String>, SshPasswordPromptError> {
        Ok(None)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchMode {
    Yes,
    No,
}

impl BatchMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Yes => "yes",
            Self::No => "no",
        }
    }
}

/// `SshAuthOptions`. `auth_secret` has the original's three states: `None` (undefined: no
/// variable), `Some(None)` (null: an empty `T3_SSH_AUTH_SECRET`) and `Some(Some(secret))`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SshAuthOptions {
    pub auth_secret: Option<Option<String>>,
    pub batch_mode: Option<BatchMode>,
    pub interactive_auth: Option<bool>,
}

/// `SshAskpassFile`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshAskpassFile {
    pub path: String,
    pub contents: &'static str,
    pub mode: Option<u32>,
}

/// `SshAskpassHelperDescriptor`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshAskpassHelperDescriptor {
    pub launcher_path: String,
    pub files: Vec<SshAskpassFile>,
}

/// The host facts `buildSshChildEnvironment` reads from its Effect context.
#[derive(Debug, Clone)]
pub struct SshAuthHost {
    pub windows: bool,
    /// `DISPLAY` of this process, read when the caller supplies no base environment.
    pub process_display: Option<String>,
    /// Where the default askpass directory is created.
    pub temp_root: PathBuf,
}

impl SshAuthHost {
    pub fn from_process() -> Self {
        Self {
            windows: cfg!(windows),
            process_display: std::env::var("DISPLAY").ok(),
            temp_root: std::env::temp_dir(),
        }
    }
}

/// `SshChildEnvironmentOptions`
#[derive(Debug, Clone, Default)]
pub struct SshChildEnvironmentOptions {
    pub interactive_auth: bool,
    /// `None` is the original's undefined: nothing is copied, and the display comes from the host.
    pub base_env: Option<BTreeMap<String, String>>,
    pub askpass_directory: Option<String>,
    pub auth_secret: Option<Option<String>>,
}

/// Node's `path.posix.join(directory, file_name)` for a plain file name.
fn posix_join(directory: &str, file_name: &str) -> String {
    let joined = if directory.is_empty() {
        file_name.to_owned()
    } else {
        format!("{directory}/{file_name}")
    };
    let absolute = joined.starts_with('/');
    let mut segments: Vec<&str> = Vec::new();
    for segment in joined.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if segments.last().is_some_and(|last| *last != "..") {
                    segments.pop();
                } else if !absolute {
                    segments.push("..");
                }
            }
            other => segments.push(other),
        }
    }
    let body = segments.join("/");
    match (absolute, body.is_empty()) {
        (true, true) => "/".to_owned(),
        (true, false) => format!("/{body}"),
        (false, true) => ".".to_owned(),
        (false, false) => body,
    }
}

/// `joinSshAskpassPath`
fn join_ssh_askpass_path(directory: &str, file_name: &str, windows: bool) -> String {
    let trimmed = directory.trim_end_matches(['\\', '/']);
    if windows {
        format!("{trimmed}\\{file_name}")
    } else {
        format!("{trimmed}/{file_name}")
    }
}

/// `buildSshAskpassHelperDescriptor`: the files that make up the askpass helper in `directory`.
pub fn build_ssh_askpass_helper_descriptor(
    directory: &str,
    windows: bool,
) -> SshAskpassHelperDescriptor {
    if windows {
        let launcher = join_ssh_askpass_path(directory, "ssh-askpass.cmd", true);
        return SshAskpassHelperDescriptor {
            launcher_path: launcher.clone(),
            files: vec![
                SshAskpassFile {
                    path: launcher,
                    contents: ASKPASS_WINDOWS_LAUNCHER_SCRIPT,
                    mode: None,
                },
                SshAskpassFile {
                    path: join_ssh_askpass_path(directory, "ssh-askpass.ps1", true),
                    contents: ASKPASS_WINDOWS_SCRIPT,
                    mode: None,
                },
            ],
        };
    }
    let path = posix_join(directory, "ssh-askpass.sh");
    SshAskpassHelperDescriptor {
        launcher_path: path.clone(),
        files: vec![SshAskpassFile {
            path,
            contents: ASKPASS_POSIX_SCRIPT,
            mode: Some(0o700),
        }],
    }
}

/// `getDefaultSshAskpassDirectory`: a fresh private directory, as `fs.makeTempDirectory` makes one.
///
/// TODO(original packages/ssh/src/auth.ts `getDefaultSshAskpassDirectory`): every interactive ssh
/// call makes a new `t3code-ssh-runtime-*` directory and nothing ever removes it, so a long session
/// that keeps prompting leaves one small directory per call under the temp root. Kept.
fn default_ssh_askpass_directory(host: &SshAuthHost) -> io::Result<String> {
    let parent = host
        .temp_root
        .join(format!("t3code-ssh-runtime-{}", uuid::Uuid::new_v4().simple()));
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&parent)?;
    Ok(parent.join(SSH_ASKPASS_DIR_NAME).to_string_lossy().into_owned())
}

/// `ensureSshAskpassHelpers`: writes any helper file that is missing or differs, and returns the
/// launcher path.
fn ensure_ssh_askpass_helpers(directory: &str, windows: bool) -> io::Result<String> {
    let descriptor = build_ssh_askpass_helper_descriptor(directory, windows);
    let launcher_directory = std::path::Path::new(&descriptor.launcher_path)
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    std::fs::create_dir_all(launcher_directory)?;
    for file in &descriptor.files {
        let current = std::fs::read_to_string(&file.path).ok();
        if current.as_deref() != Some(file.contents) {
            std::fs::write(&file.path, file.contents)?;
        }
        #[cfg(unix)]
        if let (Some(mode), false) = (file.mode, windows) {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file.path, std::fs::Permissions::from_mode(mode))?;
        }
    }
    Ok(descriptor.launcher_path)
}

/// `buildSshChildEnvironment`: the variables to add to ssh's environment. Without
/// `interactive_auth` that is the base environment unchanged (empty when none was given, because
/// the spawn inherits the process environment on top).
pub fn build_ssh_child_environment(
    input: &SshChildEnvironmentOptions,
    host: &SshAuthHost,
) -> io::Result<BTreeMap<String, String>> {
    let base_env = input.base_env.clone().unwrap_or_default();
    if !input.interactive_auth {
        return Ok(base_env);
    }
    let host_display = match &input.base_env {
        Some(base) => base.get("DISPLAY").cloned(),
        None => host.process_display.clone(),
    };
    let directory = match &input.askpass_directory {
        Some(directory) => directory.clone(),
        None => default_ssh_askpass_directory(host)?,
    };
    let ssh_askpass = ensure_ssh_askpass_helpers(&directory, host.windows)?;
    let mut env = base_env;
    env.insert("SSH_ASKPASS".to_owned(), ssh_askpass);
    env.insert("SSH_ASKPASS_REQUIRE".to_owned(), "force".to_owned());
    if let Some(secret) = &input.auth_secret {
        env.insert(
            "T3_SSH_AUTH_SECRET".to_owned(),
            secret.clone().unwrap_or_default(),
        );
    }
    let has_display = env.get("DISPLAY").is_some_and(|d| !d.is_empty())
        || host_display.is_some_and(|d| !d.is_empty());
    if !host.windows && !has_display {
        env.insert("DISPLAY".to_owned(), "t3code".to_owned());
    }
    Ok(env)
}

/// `isSshAuthFailure`, on the error's message.
pub fn is_ssh_auth_failure(message: &str) -> bool {
    static PERMISSION: OnceLock<Regex> = OnceLock::new();
    let permission = PERMISSION.get_or_init(|| {
        Regex::new(
            r"permission denied \((?:publickey|password|keyboard-interactive|hostbased|gssapi-with-mic)[^)]*\)",
        )
        .expect("permission denied pattern")
    });
    let normalized = message.to_lowercase();
    permission.is_match(&normalized)
        || normalized.contains("authentication failed")
        || normalized.contains("too many authentication failures")
}
