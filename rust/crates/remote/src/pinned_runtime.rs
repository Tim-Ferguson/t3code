//! Pinned runtimes (`apps/server/src/cloud/pinnedRuntime.ts`). A pinned runtime is an exact t3
//! release archive unpacked into `<baseDir>/runtime/versions/<version>`: the self-contained
//! executable, the web client, and the native packages beside it. The boot service points its unit
//! or launch agent at the executable, and server self-update installs the target version here before
//! switching over. The runtime never depends on a Node or npm on the machine.
//!
//! The remote runner script ([`crate::ssh_scripts`]) unpacks the same layout in shell on a remote
//! host (`~/.t3/runtime/versions/<version>/t3` plus the `.install-complete` sentinel), so that
//! `t3 service install` there reuses it; the two must keep agreeing on those names.
//!
//! [`ensure_pinned_runtime_installed`] is the install transaction: reuse a complete install, else
//! download the release's checksum file and archive, verify, extract into a `.staging-*` directory,
//! validate the staged executable, write the sentinel, and rename into place. Everything that
//! reaches outside the process goes through a trait the caller supplies ([`ReleaseFetcher`],
//! [`ProcessRunner`], [`RuntimeValidator`]), so the crate downloads nothing and runs nothing itself.

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use sha2::{Digest, Sha256};

use crate::cli_release::{
    CLI_RELEASE_CHECKSUMS_FILE, cli_archive_file_name, cli_archive_platform_key,
    cli_archive_tar_command, cli_release_download_base_url, parse_checksums,
};

const PINNED_RUNTIME_DIR: &str = "runtime";
pub const PINNED_RUNTIME_INSTALL_TIMEOUT: Duration = Duration::from_secs(600);
const PINNED_RUNTIME_ARCHIVE_FILE: &str = "t3-runtime-archive";
pub const PINNED_RUNTIME_SENTINEL_FILE: &str = ".install-complete";

/// Boot-service setup and remote update can construct separate services. Serialize the complete
/// install transaction across every caller in this process.
static INSTALL_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// `PinnedRuntimePaths`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedRuntimePaths {
    pub version_dir: PathBuf,
    /// The executable. Its existence is what marks a runtime as present.
    pub entry_path: PathBuf,
    pub sentinel_path: PathBuf,
}

/// `pinnedRuntimeVersionsDir`
pub fn pinned_runtime_versions_dir(base_dir: &Path) -> PathBuf {
    base_dir.join(PINNED_RUNTIME_DIR).join("versions")
}

/// `pinnedRuntimePaths`; `platform` is Node's (`win32` names the executable `t3.exe`).
pub fn pinned_runtime_paths(base_dir: &Path, version: &str, platform: &str) -> PinnedRuntimePaths {
    let version_dir = pinned_runtime_versions_dir(base_dir).join(version);
    PinnedRuntimePaths {
        entry_path: version_dir.join(if platform == "win32" { "t3.exe" } else { "t3" }),
        sentinel_path: version_dir.join(PINNED_RUNTIME_SENTINEL_FILE),
        version_dir,
    }
}

/// `pinnedRuntimeCommand`: the exact command that runs a pinned runtime.
pub fn pinned_runtime_command(paths: &PinnedRuntimePaths) -> (String, Vec<String>) {
    (paths.entry_path.to_string_lossy().into_owned(), Vec::new())
}

/// `PinnedRuntimeInstallError` and `PinnedRuntimePreflightBlockedError`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PinnedRuntimeError {
    #[error("{}", install_message(.step, .exit_code))]
    Install {
        step: String,
        exit_code: Option<i32>,
        stdout_length: Option<usize>,
        stderr_length: Option<usize>,
        cause: Option<String>,
    },
    #[error("{reason}")]
    PreflightBlocked { version: String, reason: String },
}

fn install_message(step: &str, exit_code: &Option<i32>) -> String {
    match exit_code {
        None => format!("Pinned runtime install failed while {step}."),
        Some(code) => format!("Pinned runtime install failed while {step} (exit code {code})."),
    }
}

impl PinnedRuntimeError {
    pub fn step(step: impl Into<String>) -> Self {
        Self::Install {
            step: step.into(),
            exit_code: None,
            stdout_length: None,
            stderr_length: None,
            cause: None,
        }
    }

    pub fn step_with_cause(step: impl Into<String>, cause: impl std::fmt::Display) -> Self {
        Self::Install {
            step: step.into(),
            exit_code: None,
            stdout_length: None,
            stderr_length: None,
            cause: Some(cause.to_string()),
        }
    }
}

/// `PinnedRuntimeProgress`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinnedRuntimeProgress {
    Download { received: u64, total: Option<u64> },
    Verify,
    Extract,
    Validate,
    Cached,
}

/// What a process run produced (`ProcessRunOutput`, the parts these flows read). A run that times
/// out or cannot start is an `Err`, as the server's runner reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessRunOutput {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// The server's `ProcessRunner`: run a command to completion with a timeout (60 s when `None`).
#[async_trait]
pub trait ProcessRunner: Send + Sync {
    async fn run(
        &self,
        command: &str,
        args: &[String],
        timeout: Option<Duration>,
    ) -> Result<ProcessRunOutput, String>;
}

/// One release asset being downloaded.
#[async_trait]
pub trait ReleaseAssetStream: Send {
    /// `content-length`, when the server sent one.
    fn content_length(&self) -> Option<u64>;
    /// The next chunk, or `None` at the end.
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, String>;
}

/// Opens release assets; a non-2xx answer is an `Err`.
#[async_trait]
pub trait ReleaseFetcher: Send + Sync {
    async fn open(&self, url: &str) -> Result<Box<dyn ReleaseAssetStream>, String>;
}

/// Runs the staged (or already installed) executable to prove it works before it is published.
#[async_trait]
pub trait RuntimeValidator: Send + Sync {
    async fn validate(&self, paths: &PinnedRuntimePaths) -> Result<(), PinnedRuntimeError>;
}

pub struct PinnedRuntimeInstallInput<'a> {
    pub base_dir: &'a Path,
    pub version: &'a str,
    pub runner: &'a dyn ProcessRunner,
    pub fetcher: &'a dyn ReleaseFetcher,
    pub validator: &'a dyn RuntimeValidator,
    pub platform: &'a str,
    pub arch: &'a str,
    pub release_base_url: Option<&'a str>,
    pub on_progress: Option<&'a (dyn Fn(PinnedRuntimeProgress) + Send + Sync)>,
    /// The environment for `cliArchiveTarCommand` (`SystemRoot` on Windows).
    pub env: &'a std::collections::BTreeMap<String, String>,
}

async fn fetch_release_asset(
    fetcher: &dyn ReleaseFetcher,
    url: &str,
    step: &str,
    on_progress: Option<&(dyn Fn(PinnedRuntimeProgress) + Send + Sync)>,
) -> Result<Vec<u8>, PinnedRuntimeError> {
    // The install lock is held for the whole transaction, so a stalled download must fail rather
    // than block every other caller.
    let work = async {
        let mut stream = fetcher
            .open(url)
            .await
            .map_err(|cause| PinnedRuntimeError::step_with_cause(step, cause))?;
        let Some(on_progress) = on_progress else {
            let mut bytes = Vec::new();
            while let Some(chunk) = stream
                .next_chunk()
                .await
                .map_err(|cause| PinnedRuntimeError::step_with_cause(step, cause))?
            {
                bytes.extend_from_slice(&chunk);
            }
            return Ok(bytes);
        };
        let total = stream.content_length().filter(|length| *length > 0);
        let mut received: u64 = 0;
        on_progress(PinnedRuntimeProgress::Download { received, total });
        let mut bytes = Vec::new();
        while let Some(chunk) = stream
            .next_chunk()
            .await
            .map_err(|cause| PinnedRuntimeError::step_with_cause(step, cause))?
        {
            received += chunk.len() as u64;
            bytes.extend_from_slice(&chunk);
            on_progress(PinnedRuntimeProgress::Download { received, total });
        }
        // A completed chunked response finally gives us its total size.
        if total.is_none() && received > 0 {
            on_progress(PinnedRuntimeProgress::Download {
                received,
                total: Some(received),
            });
        }
        Ok(bytes)
    };
    tokio::time::timeout(PINNED_RUNTIME_INSTALL_TIMEOUT, work)
        .await
        .unwrap_or_else(|_| Err(PinnedRuntimeError::step(format!("{step} (timed out)"))))
}

/// Downloads the release archive for this platform, verifies it against the release's checksum
/// file, and unpacks it so the executable sits directly in the staging directory. Only `tar` is
/// required on the host; every supported OS ships one that reads gzip and zip.
async fn install_from_archive(
    input: &PinnedRuntimeInstallInput<'_>,
    staging_dir: &Path,
) -> Result<(), PinnedRuntimeError> {
    let Some(platform_key) = cli_archive_platform_key(input.platform, input.arch) else {
        return Err(PinnedRuntimeError::step(format!(
            "selecting a t3 release archive for {}-{}",
            input.platform, input.arch
        )));
    };
    let base_url = cli_release_download_base_url(input.version, input.release_base_url);
    let file_name = cli_archive_file_name(input.version, platform_key);
    let progress = |p| {
        if let Some(on_progress) = input.on_progress {
            on_progress(p);
        }
    };

    progress(PinnedRuntimeProgress::Download {
        received: 0,
        total: None,
    });
    let checksums_bytes = fetch_release_asset(
        input.fetcher,
        &format!("{base_url}/{CLI_RELEASE_CHECKSUMS_FILE}"),
        "downloading the t3 release checksums",
        None,
    )
    .await?;
    let checksums = parse_checksums(&String::from_utf8_lossy(&checksums_bytes));
    let Some(expected) = checksums.get(&file_name) else {
        return Err(PinnedRuntimeError::step(format!(
            "finding {file_name} in the t3 release checksums"
        )));
    };
    let archive = fetch_release_asset(
        input.fetcher,
        &format!("{base_url}/{file_name}"),
        "downloading the t3 release archive",
        input.on_progress,
    )
    .await?;
    progress(PinnedRuntimeProgress::Verify);
    let digest: String = Sha256::digest(&archive)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if &digest != expected {
        return Err(PinnedRuntimeError::step(
            "verifying the t3 release archive checksum",
        ));
    }

    let archive_path = staging_dir.join(PINNED_RUNTIME_ARCHIVE_FILE);
    std::fs::write(&archive_path, &archive).map_err(|cause| {
        PinnedRuntimeError::step_with_cause("writing the t3 release archive", cause)
    })?;
    progress(PinnedRuntimeProgress::Extract);
    let extract_step = "extracting the t3 release archive";
    // The archive wraps everything in one directory named after its stem; strip it so the
    // executable lands at <versionDir>/t3.
    let result = input
        .runner
        .run(
            &cli_archive_tar_command(input.platform, input.env),
            &[
                "-xf".to_owned(),
                archive_path.to_string_lossy().into_owned(),
                "-C".to_owned(),
                staging_dir.to_string_lossy().into_owned(),
                "--strip-components=1".to_owned(),
            ],
            Some(PINNED_RUNTIME_INSTALL_TIMEOUT),
        )
        .await
        .map_err(|cause| PinnedRuntimeError::step_with_cause(extract_step, cause))?;
    if result.code != 0 {
        return Err(PinnedRuntimeError::Install {
            step: extract_step.to_owned(),
            exit_code: Some(result.code),
            stdout_length: Some(result.stdout.encode_utf16().count()),
            stderr_length: Some(result.stderr.encode_utf16().count()),
            cause: None,
        });
    }
    let _ = std::fs::remove_file(&archive_path);
    Ok(())
}

fn runtime_present(paths: &PinnedRuntimePaths, version: &str) -> bool {
    paths.entry_path.exists()
        && std::fs::read_to_string(&paths.sentinel_path)
            .is_ok_and(|sentinel| sentinel.trim() == version)
}

/// A unique `.staging-` directory inside `parent`, as `fs.makeTempDirectory({ prefix: ".staging-" })`.
fn make_staging_dir(parent: &Path) -> std::io::Result<PathBuf> {
    let dir = parent.join(format!(".staging-{}", &uuid::Uuid::new_v4().simple().to_string()[..6]));
    std::fs::create_dir(&dir)?;
    Ok(dir)
}

async fn install_pinned_runtime(
    input: &PinnedRuntimeInstallInput<'_>,
) -> Result<PinnedRuntimePaths, PinnedRuntimeError> {
    let paths = pinned_runtime_paths(input.base_dir, input.version, input.platform);
    // Complete means the sentinel names this version and the executable exists: tar writes the
    // executable before the last native package, so a killed install leaves a plausible-looking
    // but broken tree behind, and only the sentinel (written last) proves it finished.
    if runtime_present(&paths, input.version) {
        if let Some(on_progress) = input.on_progress {
            on_progress(PinnedRuntimeProgress::Cached);
        }
        input.validator.validate(&paths).await?;
        return Ok(paths);
    }
    if paths.version_dir.exists() {
        std::fs::remove_dir_all(&paths.version_dir).map_err(|cause| {
            PinnedRuntimeError::step_with_cause("removing an incomplete pinned runtime", cause)
        })?;
    }
    let versions_dir = paths
        .version_dir
        .parent()
        .expect("a version directory has a parent")
        .to_path_buf();
    let prepare = |cause: std::io::Error| {
        PinnedRuntimeError::step_with_cause("preparing the pinned runtime directory", cause)
    };
    std::fs::create_dir_all(&versions_dir).map_err(prepare)?;
    let staging_dir = make_staging_dir(&versions_dir).map_err(prepare)?;
    let staging_paths = PinnedRuntimePaths {
        entry_path: staging_dir.join(
            paths
                .entry_path
                .strip_prefix(&paths.version_dir)
                .expect("the entry is inside the version directory"),
        ),
        sentinel_path: staging_dir.join(PINNED_RUNTIME_SENTINEL_FILE),
        version_dir: staging_dir.clone(),
    };
    // Removes the staging directory however the install ends: success, failure, or the future being
    // dropped mid-download.
    struct RemoveStaging(PathBuf);
    impl Drop for RemoveStaging {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _staging = RemoveStaging(staging_dir.clone());
    let outcome = async {
        install_from_archive(input, &staging_dir).await?;
        if let Some(on_progress) = input.on_progress {
            on_progress(PinnedRuntimeProgress::Validate);
        }
        input.validator.validate(&staging_paths).await?;
        std::fs::write(&staging_paths.sentinel_path, format!("{}\n", input.version)).map_err(
            |cause| PinnedRuntimeError::step_with_cause("recording the completed install", cause),
        )?;
        let published = match std::fs::rename(&staging_dir, &paths.version_dir) {
            Ok(()) => true,
            Err(cause) => {
                // Another process may have published the same version first.
                if runtime_present(&paths, input.version) {
                    false
                } else {
                    return Err(PinnedRuntimeError::step_with_cause(
                        "publishing the pinned runtime",
                        cause,
                    ));
                }
            }
        };
        if !published {
            input.validator.validate(&paths).await?;
        }
        Ok(paths.clone())
    }
    .await;
    outcome
}

/// `ensurePinnedRuntimeInstalled`: installs the t3 release archive for `version` into the pinned
/// runtime directory unless a complete install is already there, and returns its paths.
pub async fn ensure_pinned_runtime_installed(
    input: PinnedRuntimeInstallInput<'_>,
) -> Result<PinnedRuntimePaths, PinnedRuntimeError> {
    let _transaction = INSTALL_LOCK.lock().await;
    install_pinned_runtime(&input).await
}
