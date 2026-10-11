//! The relay client is the `cloudflared` binary that carries a managed tunnel
//! (`packages/shared/src/relayClient.ts`): where it is found, and how the pinned release is
//! downloaded, verified and activated. The RPC contracts `cloud.getRelayClientStatus` and
//! `cloud.installRelayClient` (`packages/contracts/src/relayClient.ts`) are these types.

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

pub const CLOUDFLARED_VERSION: &str = "2026.5.2";
pub const CLOUDFLARED_PATH_ENV_NAME: &str = "T3CODE_CLOUDFLARED_PATH";

const INSTALL_LOCK_RETRY_COUNT: u32 = 100;
const INSTALL_LOCK_RETRY_DELAY: Duration = Duration::from_millis(100);
const INSTALL_LOCK_STALE_MS: u128 = 5 * 60 * 1_000;

/// Node's `process.platform` / `process.arch` naming, which keys the pinned release assets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostPlatform {
    pub platform: String,
    pub arch: String,
}

impl HostPlatform {
    pub fn current() -> Self {
        let platform = match std::env::consts::OS {
            "macos" => "darwin",
            "windows" => "win32",
            other => other,
        };
        let arch = match std::env::consts::ARCH {
            "aarch64" => "arm64",
            "x86_64" => "x64",
            other => other,
        };
        Self {
            platform: platform.to_owned(),
            arch: arch.to_owned(),
        }
    }

    fn executable_file_name(&self) -> &'static str {
        if self.platform == "win32" {
            "cloudflared.exe"
        } else {
            "cloudflared"
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ExecutableSource {
    Override,
    Managed,
    Path,
}

/// `RelayClientStatusSchema`: the result of `cloud.getRelayClientStatus`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum RelayClientStatus {
    #[serde(rename_all = "camelCase")]
    Available {
        executable_path: String,
        source: ExecutableSource,
        version: String,
    },
    Missing {
        version: String,
    },
    Unsupported {
        platform: String,
        arch: String,
        version: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallProgressStage {
    Checking,
    WaitingForLock,
    Downloading,
    Verifying,
    Installing,
    Validating,
    Activating,
}

/// `RelayClientInstallProgressEventSchema`: the stream `cloud.installRelayClient` emits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum InstallProgressEvent {
    Progress { stage: InstallProgressStage },
    Complete { status: RelayClientStatus },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallFailureReason {
    DownloadFailed,
    InvalidChecksum,
    InstallLocked,
    OverrideMissing,
    UnsupportedPlatform,
    ValidationFailed,
    WriteFailed,
}

/// `RelayClientInstallFailedError`: the failure of `cloud.installRelayClient`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct InstallError {
    pub reason: InstallFailureReason,
    pub message: String,
}

impl InstallError {
    fn new(reason: InstallFailureReason, message: &str) -> Self {
        Self {
            reason,
            message: message.to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseAsset {
    pub url: String,
    pub sha256: String,
    pub archive: ArchiveKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveKind {
    Binary,
    Tgz,
}

/// The pinned release asset for a platform, or `None` when there is no managed binary for it.
pub fn release_asset(host: &HostPlatform) -> Option<ReleaseAsset> {
    let base = "https://github.com/cloudflare/cloudflared/releases/download/2026.5.2";
    let (file, sha256, archive) = match (host.platform.as_str(), host.arch.as_str()) {
        ("darwin", "arm64") => (
            "cloudflared-darwin-arm64.tgz",
            "ba94054c9fd4297645093d59d51442e5e546d07bb0516120e694a13d5b216d38",
            ArchiveKind::Tgz,
        ),
        ("darwin", "x64") => (
            "cloudflared-darwin-amd64.tgz",
            "7240f709506bc2c1eb9da4d89cf2555499c60280ecb854b7d80e8f17d4b7903d",
            ArchiveKind::Tgz,
        ),
        ("linux", "arm64") => (
            "cloudflared-linux-arm64",
            "5a4e8ce2701105271412059f44b6a0bf1ae4542b4d98ff3180c0c019443a5815",
            ArchiveKind::Binary,
        ),
        ("linux", "x64") => (
            "cloudflared-linux-amd64",
            "5286698547f03df745adb2355f04c12dde52ef425491e81f433642d695521886",
            ArchiveKind::Binary,
        ),
        ("win32", "x64") => (
            "cloudflared-windows-amd64.exe",
            "20b9638f685333d623798e733effbad2487093f15ba592f6c7752360ff3b7ab7",
            ArchiveKind::Binary,
        ),
        _ => return None,
    };
    Some(ReleaseAsset {
        url: format!("{base}/{file}"),
        sha256: sha256.to_owned(),
        archive,
    })
}

/// What the endpoint runtime and the RPCs need from the relay client.
#[async_trait]
pub trait RelayClientResolver: Send + Sync {
    async fn resolve(&self) -> RelayClientStatus;
    /// Installs the pinned release, reporting each stage. Returns the available client.
    async fn install_with_progress(
        &self,
        report: &(dyn Fn(InstallProgressStage) + Send + Sync),
    ) -> Result<RelayClientStatus, InstallError>;
}

pub struct CloudflaredOptions {
    /// The T3 home; the managed copy lives under `tools/cloudflared/<version>/<platform>-<arch>/`.
    pub base_dir: PathBuf,
    pub host: HostPlatform,
    /// `T3CODE_CLOUDFLARED_PATH` (blank counts as unset).
    pub executable_override: Option<String>,
    /// The `PATH` to search.
    pub path_env: Option<String>,
    /// Replaces the pinned release (tests point it at a loopback server).
    pub release_asset: Option<ReleaseAsset>,
}

pub struct Cloudflared {
    options: CloudflaredOptions,
    release_asset: Option<ReleaseAsset>,
    managed_path: PathBuf,
    http: reqwest::Client,
    install_lock: Mutex<()>,
}

impl Cloudflared {
    pub fn new(options: CloudflaredOptions) -> Self {
        let release_asset = options
            .release_asset
            .clone()
            .or_else(|| release_asset(&options.host));
        let managed_path = options
            .base_dir
            .join("tools")
            .join("cloudflared")
            .join(CLOUDFLARED_VERSION)
            .join(format!("{}-{}", options.host.platform, options.host.arch))
            .join(options.host.executable_file_name());
        Self {
            options,
            release_asset,
            managed_path,
            http: reqwest::Client::new(),
            install_lock: Mutex::new(()),
        }
    }

    pub fn managed_path(&self) -> &Path {
        &self.managed_path
    }

    fn is_executable_file(&self, path: &Path) -> bool {
        let Ok(meta) = std::fs::metadata(path) else {
            return false;
        };
        if !meta.is_file() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            meta.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        {
            true
        }
    }

    fn override_path(&self) -> Option<&str> {
        self.options
            .executable_override
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }

    fn resolve_path_executable(&self) -> Option<PathBuf> {
        let path_value = self
            .options
            .path_env
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())?;
        let delimiter = if self.options.host.platform == "win32" {
            ';'
        } else {
            ':'
        };
        for directory in path_value.split(delimiter) {
            let trimmed = directory.trim().trim_matches('"');
            if trimmed.is_empty() {
                continue;
            }
            let candidate = Path::new(trimmed).join(self.options.host.executable_file_name());
            if self.is_executable_file(&candidate) {
                return Some(candidate);
            }
        }
        None
    }

    fn resolve_now(&self) -> RelayClientStatus {
        let version = CLOUDFLARED_VERSION.to_owned();
        if let Some(path) = self.override_path() {
            return if self.is_executable_file(Path::new(path)) {
                RelayClientStatus::Available {
                    executable_path: path.to_owned(),
                    source: ExecutableSource::Override,
                    version,
                }
            } else {
                RelayClientStatus::Missing { version }
            };
        }
        if self.is_executable_file(&self.managed_path) {
            return RelayClientStatus::Available {
                executable_path: self.managed_path.display().to_string(),
                source: ExecutableSource::Managed,
                version,
            };
        }
        if let Some(path) = self.resolve_path_executable() {
            return RelayClientStatus::Available {
                executable_path: path.display().to_string(),
                source: ExecutableSource::Path,
                version,
            };
        }
        if self.release_asset.is_some() {
            RelayClientStatus::Missing { version }
        } else {
            RelayClientStatus::Unsupported {
                platform: self.options.host.platform.clone(),
                arch: self.options.host.arch.clone(),
                version,
            }
        }
    }

    async fn acquire_install_lock(&self, lock_path: &Path) -> Result<(), InstallError> {
        let write_failed = || {
            InstallError::new(
                InstallFailureReason::WriteFailed,
                "Could not acquire the relay client installation lock.",
            )
        };
        for _ in 0..INSTALL_LOCK_RETRY_COUNT {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(lock_path)
            {
                Ok(_) => return Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(_) => return Err(write_failed()),
            }
            let age_ms = std::fs::metadata(lock_path)
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .map(|elapsed| elapsed.as_millis());
            if age_ms.is_some_and(|age| age > INSTALL_LOCK_STALE_MS) {
                let _ = std::fs::remove_file(lock_path);
                continue;
            }
            tokio::time::sleep(INSTALL_LOCK_RETRY_DELAY).await;
        }
        Err(InstallError::new(
            InstallFailureReason::InstallLocked,
            "Another relay client installation is still in progress.",
        ))
    }

    async fn run_command(&self, command: &Path, args: &[&str]) -> Result<(), ()> {
        let status = tokio::process::Command::new(command)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .map_err(|_| ())?;
        status.success().then_some(()).ok_or(())
    }

    async fn download(
        &self,
        asset: &ReleaseAsset,
        report: &(dyn Fn(InstallProgressStage) + Send + Sync),
    ) -> Result<Vec<u8>, InstallError> {
        report(InstallProgressStage::Downloading);
        let response = self
            .http
            .get(&asset.url)
            .send()
            .await
            .and_then(|response| response.error_for_status())
            .map_err(|_| {
                InstallError::new(
                    InstallFailureReason::DownloadFailed,
                    "Could not download the relay client.",
                )
            })?;
        let bytes = response.bytes().await.map_err(|_| {
            InstallError::new(
                InstallFailureReason::DownloadFailed,
                "Could not read the downloaded relay client binary.",
            )
        })?;
        report(InstallProgressStage::Verifying);
        let digest = Sha256::digest(&bytes);
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        if hex != asset.sha256 {
            return Err(InstallError::new(
                InstallFailureReason::InvalidChecksum,
                "Downloaded relay client checksum did not match the pinned release.",
            ));
        }
        Ok(bytes.to_vec())
    }

    async fn install_unlocked(
        &self,
        report: &(dyn Fn(InstallProgressStage) + Send + Sync),
    ) -> Result<RelayClientStatus, InstallError> {
        report(InstallProgressStage::Checking);
        let existing = self.resolve_now();
        if matches!(existing, RelayClientStatus::Available { .. }) {
            return Ok(existing);
        }
        if self.override_path().is_some() {
            return Err(InstallError::new(
                InstallFailureReason::OverrideMissing,
                &format!("{CLOUDFLARED_PATH_ENV_NAME} does not point to an executable file."),
            ));
        }
        let Some(asset) = self.release_asset.clone() else {
            return Err(InstallError::new(
                InstallFailureReason::UnsupportedPlatform,
                &format!(
                    "T3 Code does not provide a managed relay client binary for {}-{}.",
                    self.options.host.platform, self.options.host.arch
                ),
            ));
        };
        let write_failed =
            |message: &str| InstallError::new(InstallFailureReason::WriteFailed, message);
        let managed_directory = self
            .managed_path
            .parent()
            .expect("managed path has a parent");
        std::fs::create_dir_all(managed_directory)
            .map_err(|_| write_failed("Could not create the relay client tool directory."))?;
        let lock_path = PathBuf::from(format!("{}.lock", self.managed_path.display()));
        report(InstallProgressStage::WaitingForLock);
        self.acquire_install_lock(&lock_path).await?;
        let result = self.install_locked(&asset, managed_directory, report).await;
        let _ = std::fs::remove_file(&lock_path);
        result
    }

    async fn install_locked(
        &self,
        asset: &ReleaseAsset,
        managed_directory: &Path,
        report: &(dyn Fn(InstallProgressStage) + Send + Sync),
    ) -> Result<RelayClientStatus, InstallError> {
        let write_failed =
            |message: &str| InstallError::new(InstallFailureReason::WriteFailed, message);
        let after_lock = self.resolve_now();
        if matches!(after_lock, RelayClientStatus::Available { .. }) {
            return Ok(after_lock);
        }
        let temp = tempdir_in(managed_directory)
            .map_err(|_| write_failed("Could not install the relay client."))?;
        let file_name = self.options.host.executable_file_name();
        let archive_path = temp.join(if asset.archive == ArchiveKind::Tgz {
            "cloudflared.tgz"
        } else {
            file_name
        });
        let download = self.download(asset, report).await;
        let result = match download {
            Ok(bytes) => {
                self.finish_install(asset, &temp, &archive_path, &bytes, report)
                    .await
            }
            Err(error) => Err(error),
        };
        let _ = std::fs::remove_dir_all(&temp);
        result
    }

    async fn finish_install(
        &self,
        asset: &ReleaseAsset,
        temp: &Path,
        archive_path: &Path,
        bytes: &[u8],
        report: &(dyn Fn(InstallProgressStage) + Send + Sync),
    ) -> Result<RelayClientStatus, InstallError> {
        let write_failed =
            |message: &str| InstallError::new(InstallFailureReason::WriteFailed, message);
        report(InstallProgressStage::Installing);
        std::fs::write(archive_path, bytes)
            .map_err(|_| write_failed("Could not write the relay client download."))?;
        let executable = temp.join(self.options.host.executable_file_name());
        if asset.archive == ArchiveKind::Tgz {
            self.run_command(
                Path::new("tar"),
                &[
                    "-xzf",
                    &archive_path.display().to_string(),
                    "-C",
                    &temp.display().to_string(),
                ],
            )
            .await
            .map_err(|_| write_failed("Could not extract the relay client."))?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
                .map_err(|_| write_failed("Could not make the relay client executable."))?;
        }
        report(InstallProgressStage::Validating);
        self.run_command(&executable, &["version"])
            .await
            .map_err(|_| {
                InstallError::new(
                    InstallFailureReason::ValidationFailed,
                    "The downloaded relay client binary did not run.",
                )
            })?;
        report(InstallProgressStage::Activating);
        let staged = PathBuf::from(format!(
            "{}.{}.tmp",
            self.managed_path.display(),
            std::process::id()
        ));
        std::fs::rename(&executable, &staged)
            .map_err(|_| write_failed("Could not stage the relay client."))?;
        let activated = std::fs::rename(&staged, &self.managed_path);
        let _ = std::fs::remove_file(&staged);
        activated.map_err(|_| write_failed("Could not activate the relay client."))?;
        Ok(RelayClientStatus::Available {
            executable_path: self.managed_path.display().to_string(),
            source: ExecutableSource::Managed,
            version: CLOUDFLARED_VERSION.to_owned(),
        })
    }
}

fn tempdir_in(parent: &Path) -> std::io::Result<PathBuf> {
    let name = format!(".install-{}-{}", std::process::id(), rand::random::<u64>());
    let path = parent.join(name);
    std::fs::create_dir(&path)?;
    Ok(path)
}

#[async_trait]
impl RelayClientResolver for Cloudflared {
    async fn resolve(&self) -> RelayClientStatus {
        self.resolve_now()
    }

    async fn install_with_progress(
        &self,
        report: &(dyn Fn(InstallProgressStage) + Send + Sync),
    ) -> Result<RelayClientStatus, InstallError> {
        let _guard = self.install_lock.lock().await;
        self.install_unlocked(report).await
    }
}
