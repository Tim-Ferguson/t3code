//! Server-owned ACP catalog and managed provider executables.
use crate::acp_registry_archives::{self, ArchiveKind};
use crate::acp_registry_packages::{self, Distribution, PackageInstaller, Receipt};
use crate::provider_process::ProcessOptions;
use indexmap::IndexMap;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use t3_contracts::{
    AcpRegistryDistributionPreference, AcpRegistrySettings, AcpRegistrySettingsSource,
};
const REGISTRY_URL: &str = "https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json";
const MAX_REGISTRY: usize = 1024 * 1024;
const MAX_ARCHIVE: usize = 1024 * 1024 * 1024;
#[derive(Debug, thiserror::Error)]
#[error("{reason}: {detail}")]
pub struct RegistryError {
    pub reason: &'static str,
    pub detail: String,
}
fn error(reason: &'static str, detail: impl ToString) -> RegistryError {
    RegistryError {
        reason,
        detail: detail.to_string(),
    }
}
#[derive(Clone, Debug)]
pub struct BinaryTarget {
    pub archive: String,
    pub command: String,
    pub sha256: Option<String>,
    pub args: Vec<String>,
    pub environment: IndexMap<String, String>,
}
#[derive(Clone, Debug)]
pub struct PackageTarget {
    pub package: String,
    pub args: Vec<String>,
    pub environment: IndexMap<String, String>,
}
#[derive(Clone, Copy)]
enum SelectedDistribution<'a> {
    Binary(&'a BinaryTarget),
    Npm(&'a PackageTarget),
    Uv(&'a PackageTarget),
}
impl<'a> SelectedDistribution<'a> {
    fn tag(self) -> &'static str {
        match self {
            Self::Binary(_) => "binary",
            Self::Npm(_) => "npx",
            Self::Uv(_) => "uvx",
        }
    }
    fn args(self) -> &'a [String] {
        match self {
            Self::Binary(target) => &target.args,
            Self::Npm(target) | Self::Uv(target) => &target.args,
        }
    }
    fn environment(self) -> &'a IndexMap<String, String> {
        match self {
            Self::Binary(target) => &target.environment,
            Self::Npm(target) | Self::Uv(target) => &target.environment,
        }
    }
}
#[derive(Clone, Debug)]
pub struct Agent {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub authors: Vec<String>,
    pub license: Option<String>,
    pub website: Option<String>,
    pub repository: Option<String>,
    pub icon: Option<String>,
    pub binaries: IndexMap<String, BinaryTarget>,
    pub npx: Option<PackageTarget>,
    pub uvx: Option<PackageTarget>,
}
#[derive(Clone, Debug)]
struct Index {
    version: String,
    agents: Vec<Agent>,
}
fn string(value: &Value, max: usize, trim: bool, nonempty: bool) -> Result<String, RegistryError> {
    let text = value
        .as_str()
        .ok_or_else(|| error("registry_unavailable", "Expected a registry string."))?;
    let text = if trim {
        t3_contracts::trim_wire_string(text)
    } else {
        text
    };
    if text.encode_utf16().count() > max || (nonempty && text.is_empty()) {
        return Err(error(
            "registry_unavailable",
            "Registry string outside source bounds.",
        ));
    }
    Ok(text.into())
}
fn version(value: &Value) -> Result<String, RegistryError> {
    let text = string(value, 128, true, true)?;
    if !text
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphanumeric)
        || !text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
    {
        return Err(error("registry_unavailable", "Unsafe registry version."));
    }
    Ok(text)
}
fn https(value: &Value) -> Result<String, RegistryError> {
    let text = string(value, 2048, false, false)?;
    let parsed = url::Url::parse(&text)
        .map_err(|_| error("registry_unavailable", "Invalid registry URL."))?;
    if parsed.scheme() != "https" || !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(error(
            "registry_unavailable",
            "Registry URLs must use HTTPS without credentials.",
        ));
    }
    Ok(text)
}
fn optional<T>(
    value: &Value,
    key: &str,
    decode: impl FnOnce(&Value) -> Result<T, RegistryError>,
) -> Result<Option<T>, RegistryError> {
    value.get(key).map(decode).transpose()
}
fn strings(
    value: &Value,
    max_items: usize,
    max_units: usize,
) -> Result<Vec<String>, RegistryError> {
    let values = value
        .as_array()
        .filter(|values| values.len() <= max_items)
        .ok_or_else(|| error("registry_unavailable", "Invalid registry array."))?;
    values
        .iter()
        .map(|value| string(value, max_units, false, false))
        .collect()
}
fn arguments(value: &Value) -> Result<Vec<String>, RegistryError> {
    optional(value, "args", |value| strings(value, 64, 1024)).map(Option::unwrap_or_default)
}
fn environment(value: &Value) -> Result<IndexMap<String, String>, RegistryError> {
    optional(value, "env", |value| {
        let values = value
            .as_object()
            .ok_or_else(|| error("registry_unavailable", "Invalid registry environment."))?;
        values
            .iter()
            .map(|(key, value)| {
                string(&json!(key), 256, false, false)?;
                Ok((key.clone(), string(value, 1024, false, false)?))
            })
            .collect()
    })
    .map(Option::unwrap_or_default)
}
fn package(value: &Value, npx: bool) -> Result<PackageTarget, RegistryError> {
    if !value.is_object() {
        return Err(error("registry_unavailable", "Invalid package recipe."));
    }
    let package = string(&value["package"], 256, false, false)?;
    let suffix = r"v?[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?";
    // ECMAScript /u ignores ASCII case and its Kelvin-sign/long-s folds, but
    // its whitespace set excludes NEL and includes BOM. Rust \s is different.
    let whitespace = r"\t\n\x0B\f\r \u{00A0}\u{1680}\u{2000}-\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}";
    // JavaScript /u without /m rejects a final line terminator.
    let end = r"\z";
    let pattern = if npx {
        format!(
            r"(?i)\A(?:@[^/@{whitespace}]+/[^@{whitespace}]+|[a-z0-9][a-z0-9._-]*)@{suffix}{end}"
        )
    } else {
        format!(r"(?i)\A[a-z0-9][a-z0-9._-]*(?:@|==){suffix}{end}")
    };
    if !regex::Regex::new(&pattern).unwrap().is_match(&package) {
        return Err(error(
            "registry_unavailable",
            "Registry package must be exactly pinned.",
        ));
    }
    Ok(PackageTarget {
        package,
        args: arguments(value)?,
        environment: environment(value)?,
    })
}
fn decode_agent(value: &Value) -> Result<Agent, RegistryError> {
    if !value.is_object() {
        return Err(error("registry_unavailable", "Invalid registry agent."));
    }
    let id = string(&value["id"], 128, false, true)?;
    if !id
        .as_bytes()
        .first()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        || !id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
    {
        return Err(error("registry_unavailable", "Invalid registry agent ID."));
    }
    let distribution = value["distribution"]
        .as_object()
        .ok_or_else(|| error("registry_unavailable", "Invalid distribution."))?;
    let binaries = distribution
        .get("binary")
        .map(|value| {
            let entries = value
                .as_object()
                .ok_or_else(|| error("registry_unavailable", "Invalid binary targets."))?;
            entries
                .iter()
                .map(|(platform, value)| {
                    if !value.is_object() {
                        return Err(error("registry_unavailable", "Invalid binary target."));
                    }
                    let sha = optional(value, "sha256", |value| {
                        let sha = string(value, 64, false, false)?;
                        if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
                            return Err(error("registry_unavailable", "Invalid SHA256."));
                        }
                        Ok(sha)
                    })?;
                    Ok((
                        platform.clone(),
                        BinaryTarget {
                            archive: https(&value["archive"])?,
                            command: string(&value["cmd"], 1024, false, false)?,
                            sha256: sha,
                            args: arguments(value)?,
                            environment: environment(value)?,
                        },
                    ))
                })
                .collect::<Result<IndexMap<_, _>, RegistryError>>()
        })
        .transpose()?
        .unwrap_or_default();
    Ok(Agent {
        id,
        name: string(&value["name"], 160, true, true)?,
        version: version(&value["version"])?,
        description: string(&value["description"], 1024, false, false)?,
        authors: optional(value, "authors", |value| strings(value, 16, 256))?.unwrap_or_default(),
        license: optional(value, "license", |value| string(value, 128, false, false))?,
        website: optional(value, "website", https)?,
        repository: optional(value, "repository", https)?,
        icon: optional(value, "icon", https)?,
        binaries,
        npx: optional(&value["distribution"], "npx", |value| package(value, true))?,
        uvx: optional(&value["distribution"], "uvx", |value| package(value, false))?,
    })
}
fn decode_index(bytes: &[u8]) -> Result<Index, RegistryError> {
    if bytes.len() > MAX_REGISTRY {
        return Err(error(
            "registry_unavailable",
            "ACP Registry index exceeds1048576 bytes.",
        ));
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| error("registry_unavailable", "Registry index is not valid UTF8."))?;
    let value: Value = serde_json::from_str(text).map_err(|e| error("registry_unavailable", e))?;
    let version = version(&value["version"])?;
    let raw = value["agents"]
        .as_array()
        .filter(|v| v.len() <= 512)
        .ok_or_else(|| error("registry_unavailable", "Invalid registry index agents."))?;
    let agents = raw
        .iter()
        .filter_map(|value| decode_agent(value).ok())
        .collect::<Vec<_>>();
    if agents.len() != raw.len() {
        tracing::warn!(
            discarded = raw.len() - agents.len(),
            "ignored invalid ACP Registry entries"
        );
    }
    Ok(Index { version, agents })
}
pub fn platform_target(platform: &str, architecture: &str) -> Option<String> {
    let platform = match platform {
        "darwin" => "darwin",
        "linux" => "linux",
        "win32" => "windows",
        _ => return None,
    };
    let arch = match architecture {
        "arm64" => "aarch64",
        "x64" => "x86_64",
        _ => return None,
    };
    Some(format!("{platform}-{arch}"))
}
fn host_target() -> Option<String> {
    platform_target(
        if cfg!(target_os = "macos") {
            "darwin"
        } else if cfg!(target_os = "linux") {
            "linux"
        } else if cfg!(windows) {
            "win32"
        } else {
            "unknown"
        },
        if cfg!(target_arch = "aarch64") {
            "arm64"
        } else if cfg!(target_arch = "x86_64") {
            "x64"
        } else {
            "unknown"
        },
    )
}
#[derive(Clone, Debug)]
pub struct Catalog(Arc<Inner>);
#[derive(Debug)]
struct Inner {
    cache_dir: PathBuf,
    tools_dir: PathBuf,
    url: String,
    http: reqwest::Client,
    index: Mutex<Option<Index>>,
    revision: AtomicU64,
    refresh: tokio::sync::Mutex<()>,
    install: Arc<tokio::sync::Mutex<()>>,
    reservations: Mutex<HashMap<String, std::time::Instant>>,
    target: Option<String>,
    #[cfg(test)]
    assets: Mutex<HashMap<String, Vec<u8>>>,
    #[cfg(test)]
    downloads: AtomicU64,
    #[cfg(test)]
    archive_environment: Mutex<Option<IndexMap<String, String>>>,
    #[cfg(test)]
    delete_gate: Mutex<
        Option<(
            tokio::sync::oneshot::Sender<()>,
            std::sync::mpsc::Receiver<()>,
        )>,
    >,
}
impl Catalog {
    pub fn new(cache_dir: PathBuf, tools_dir: PathBuf) -> Result<Self, RegistryError> {
        Self::with_origin(cache_dir, tools_dir, REGISTRY_URL.into())
    }
    /// The original catalog factory allows overriding its registry URL; callers
    /// still must supply credential-free HTTPS, including test transport hooks.
    pub fn with_origin(
        cache_dir: PathBuf,
        tools_dir: PathBuf,
        url: String,
    ) -> Result<Self, RegistryError> {
        https(&json!(url))?;
        let http = reqwest::Client::builder()
            .build()
            .map_err(|e| error("registry_unavailable", e))?;
        Ok(Self(Arc::new(Inner {
            cache_dir: cache_dir.join("acp-registry"),
            tools_dir,
            url,
            http,
            index: Mutex::new(None),
            revision: AtomicU64::new(0),
            refresh: tokio::sync::Mutex::new(()),
            install: Arc::new(tokio::sync::Mutex::new(())),
            reservations: Mutex::new(HashMap::new()),
            target: host_target(),
            #[cfg(test)]
            assets: Mutex::new(HashMap::new()),
            #[cfg(test)]
            downloads: AtomicU64::new(0),
            #[cfg(test)]
            archive_environment: Mutex::new(None),
            #[cfg(test)]
            delete_gate: Mutex::new(None),
        })))
    }
    async fn disk_cached(&self) -> Result<Index, RegistryError> {
        let bytes = tokio::fs::read(self.0.cache_dir.join("registry.json"))
            .await
            .map_err(|_| {
                error(
                    "registry_unavailable",
                    "No valid cached ACP Registry index is available.",
                )
            })?;
        decode_index(&bytes)
    }
    async fn cached(&self) -> Result<Index, RegistryError> {
        if let Some(index) = self.0.index.lock().unwrap().clone() {
            return Ok(index);
        }
        let index = self.disk_cached().await?;
        *self.0.index.lock().unwrap() = Some(index.clone());
        Ok(index)
    }
    async fn registry(&self) -> Result<Index, RegistryError> {
        match self.cached().await {
            Ok(index) => Ok(index),
            Err(_) => self.refresh().await,
        }
    }
    async fn fetch(&self) -> Result<Index, RegistryError> {
        let bytes = self
            .download(
                &self.0.url,
                MAX_REGISTRY,
                Duration::from_secs(30),
                "registry_unavailable",
            )
            .await?;
        let index = decode_index(&bytes)?;
        // Source writeRegistryCache ignores write failures. Keep the complete
        // write/rename operation with its temporary-file guard even if the
        // awaiting refresh is cancelled while an OS filesystem call is pending.
        let cache_dir = self.0.cache_dir.clone();
        let _ = tokio::task::spawn_blocking(move || {
            let temporary = CacheTemporary(cache_dir.join(format!(
                "registry.json.{}-{}.tmp",
                std::process::id(),
                uuid::Uuid::new_v4()
            )));
            std::fs::create_dir_all(&cache_dir)?;
            std::fs::write(&temporary.0, bytes)?;
            std::fs::rename(&temporary.0, cache_dir.join("registry.json"))
        })
        .await;
        Ok(index)
    }
    async fn refresh(&self) -> Result<Index, RegistryError> {
        self.refresh_observed(self.0.revision.load(Ordering::Acquire))
            .await
    }
    async fn refresh_observed(&self, observed: u64) -> Result<Index, RegistryError> {
        let _permit = self.0.refresh.lock().await;
        if self.0.revision.load(Ordering::Acquire) != observed {
            if let Some(current) = self.0.index.lock().unwrap().clone() {
                return Ok(current);
            }
        }
        // Fallback covers transport and decoder failures. It reads disk, rather than
        // substituting a possibly stale in-memory index, just as readCachedRegistry does.
        let index = match self.fetch().await {
            Ok(index) => index,
            Err(network) => self.disk_cached().await.or(Err(network))?,
        };
        *self.0.index.lock().unwrap() = Some(index.clone());
        self.0.revision.fetch_add(1, Ordering::Release);
        Ok(index)
    }
    async fn download(
        &self,
        url: &str,
        max: usize,
        timeout: Duration,
        reason: &'static str,
    ) -> Result<Vec<u8>, RegistryError> {
        https(&json!(url))?;
        #[cfg(test)]
        self.0.downloads.fetch_add(1, Ordering::Relaxed);
        #[cfg(test)]
        if let Some(bytes) = self.0.assets.lock().unwrap().get(url).cloned() {
            if bytes.len() > max {
                return Err(error(reason, "Response exceeds source byte bound."));
            }
            return Ok(bytes);
        }
        tokio::time::timeout(timeout, async {
            let mut response = self
                .0
                .http
                .get(url)
                .send()
                .await
                .map_err(|e| error(reason, e))?
                .error_for_status()
                .map_err(|e| error(reason, e))?;
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|e| error(reason, e))? {
                if bytes.len() + chunk.len() > max {
                    return Err(error(reason, "Response exceeds source byte bound."));
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(bytes)
        })
        .await
        .map_err(|_| error(reason, "Timed out downloading ACP registry data."))?
    }
    async fn download_file(
        &self,
        url: &str,
        destination: &Path,
        cleanup: Arc<Temporary>,
    ) -> Result<String, RegistryError> {
        https(&json!(url))?;
        let destination = destination.to_owned();
        // The opened file owns the directory/lock guard. Even cancellation of
        // this await cannot detach file creation from its cleanup ownership.
        let file = tokio::task::spawn_blocking(move || {
            let file = std::fs::File::create(destination)?;
            Ok::<_, std::io::Error>(Arc::new(Mutex::new(InstallArchive {
                file,
                _cleanup: cleanup,
            })))
        })
        .await
        .map_err(|e| error("install_failed", e))?
        .map_err(|e| error("install_failed", e))?;
        let mut hash = Sha256::new();
        #[cfg(test)]
        let fixture = { self.0.assets.lock().unwrap().get(url).cloned() };
        #[cfg(test)]
        if let Some(bytes) = fixture {
            if bytes.len() > MAX_ARCHIVE {
                return Err(error(
                    "archive_invalid",
                    "Archive exceeds source byte bound.",
                ));
            }
            hash.update(&bytes);
            write_archive(file.clone(), bytes).await?;
            return Ok(format!("{:x}", hash.finalize()));
        }
        tokio::time::timeout(Duration::from_secs(20 * 60), async {
            let mut response = self
                .0
                .http
                .get(url)
                .send()
                .await
                .map_err(|e| error("download_failed", e))?
                .error_for_status()
                .map_err(|e| error("download_failed", e))?;
            let mut total = 0usize;
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|e| error("download_failed", e))?
            {
                total = total
                    .checked_add(chunk.len())
                    .filter(|size| *size <= MAX_ARCHIVE)
                    .ok_or_else(|| {
                        error("archive_invalid", "Archive exceeds source byte bound.")
                    })?;
                hash.update(&chunk);
                write_archive(file.clone(), chunk.to_vec()).await?;
            }
            Ok(format!("{:x}", hash.finalize()))
        })
        .await
        .map_err(|_| {
            error(
                "download_failed",
                "Timed out downloading ACP Registry binary.",
            )
        })?
    }
    fn distribution<'a>(
        &self,
        agent: &'a Agent,
        preference: AcpRegistryDistributionPreference,
    ) -> Result<SelectedDistribution<'a>, RegistryError> {
        let binary = self
            .0
            .target
            .as_ref()
            .and_then(|target| agent.binaries.get(target));
        if matches!(
            preference,
            AcpRegistryDistributionPreference::Auto | AcpRegistryDistributionPreference::Binary
        ) {
            if let Some(target) = binary {
                return Ok(SelectedDistribution::Binary(target));
            }
        }
        if matches!(
            preference,
            AcpRegistryDistributionPreference::Auto | AcpRegistryDistributionPreference::Npx
        ) {
            if let Some(target) = &agent.npx {
                return Ok(SelectedDistribution::Npm(target));
            }
        }
        if matches!(
            preference,
            AcpRegistryDistributionPreference::Auto | AcpRegistryDistributionPreference::Uvx
        ) {
            if let Some(target) = &agent.uvx {
                return Ok(SelectedDistribution::Uv(target));
            }
        }
        Err(error(
            if self.0.target.is_none() {
                "unsupported_platform"
            } else {
                "unsupported_distribution"
            },
            "No compatible ACP Registry distribution matches this platform and preference.",
        ))
    }
    #[cfg(test)]
    fn binary<'a>(
        &self,
        agent: &'a Agent,
        preference: AcpRegistryDistributionPreference,
    ) -> Result<&'a BinaryTarget, RegistryError> {
        match self.distribution(agent, preference)? {
            SelectedDistribution::Binary(target) => Ok(target),
            _ => Err(error(
                "unsupported_distribution",
                "Requested distribution is not a binary.",
            )),
        }
    }
    fn host_environment(&self) -> IndexMap<String, String> {
        #[cfg(test)]
        if let Some(environment) = self.0.archive_environment.lock().unwrap().clone() {
            return environment;
        }
        std::env::vars().collect()
    }
    async fn install_package_locked(
        &self,
        agent: &Agent,
        target: &PackageTarget,
        distribution: Distribution,
        environment: IndexMap<String, String>,
        admission: Arc<dyn Send + Sync>,
    ) -> Result<Receipt, RegistryError> {
        let root = self
            .0
            .tools_dir
            .join(&agent.id)
            .join(encode_version(&agent.version));
        let receipts = self.0.cache_dir.join("package-installs");
        let resolve = Arc::new(|command: &str, environment: &IndexMap<String, String>| {
            if !command.contains('/')
                && !command.contains('\\')
                && !has_environment_path(environment.keys().map(String::as_str))
            {
                return None;
            }
            executable(
                command,
                &environment
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect(),
            )
        });
        PackageInstaller {
            agent,
            distribution,
            spec: &target.package,
            agent_root: &root,
            receipts: &receipts,
            environment,
            resolve,
            installation: admission,
        }
        .ensure()
        .await
    }
    async fn install_package(
        &self,
        agent: &Agent,
        target: &PackageTarget,
        distribution: Distribution,
        environment: &HashMap<String, String>,
    ) -> Result<Receipt, RegistryError> {
        let admission = Arc::new(self.0.install.clone().lock_owned().await);
        self.install_package_locked(
            agent,
            target,
            distribution,
            environment
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            admission,
        )
        .await
    }
    /// Search performs a refresh, filters unavailable distributions/managers,
    /// and applies the source relevance/name/id ordering before the20-row cap.
    /// Source removal is serialized against settings reference changes and
    /// installs. The native job retains both leases after caller cancellation.
    pub async fn uninstall_managed_binary(
        &self,
        input: &t3_contracts::AcpRegistryManagedBinaryUninstallInput,
        service: &crate::server_settings::SettingsService,
    ) -> Result<t3_contracts::AcpRegistryManagedBinaryUninstallResult, RegistryError> {
        let catalog = self.clone();
        let agent_id = input.agent_id.clone();
        service.with_settings_snapshot_lease(move |settings, lease| async move {
            let permit = catalog.0.install.clone().lock_owned().await;
            let id = agent_id.as_str();
            if id.is_empty() || id.encode_utf16().count() > 128 || !id.as_bytes()[0].is_ascii_lowercase() && !id.as_bytes()[0].is_ascii_digit() || !id.bytes().all(|value| value.is_ascii_lowercase() || value.is_ascii_digit() || b"._-".contains(&value)) {
                return Err(error("install_failed", "ACP Registry managed binary uninstall received an invalid agent ID."));
            }
            if crate::acp_registry_uninstall::referenced(&settings,id) {
                catalog.0.reservations.lock().unwrap().remove(id);
                return Ok(t3_contracts::AcpRegistryManagedBinaryUninstallResult {agent_id, removed:false});
            }
            {
                let mut reservations = catalog.0.reservations.lock().unwrap();
                if reservations.get(id).is_some_and(|expires| *expires > std::time::Instant::now()) {
                    return Ok(t3_contracts::AcpRegistryManagedBinaryUninstallResult {agent_id, removed:false});
                }
                reservations.remove(id);
            }
            let root = catalog.0.tools_dir.join(id);
            let removed = tokio::task::spawn_blocking(move || {
                let _ownership = (lease, permit);
                #[cfg(test)]
                if let Some((entered, released)) = catalog.0.delete_gate.lock().unwrap().take() {
                    let _ = entered.send(());
                    let _ = released.recv();
                }
                crate::acp_registry_uninstall::remove_binary_directories(&root)
            }).await.map_err(|cause| error("install_failed",cause))?.map_err(|cause| error("install_failed",cause))?;
            Ok(t3_contracts::AcpRegistryManagedBinaryUninstallResult {agent_id, removed})
        }).await.map_err(|_| error("install_failed", "Could not read provider settings while checking managed ACP binary references."))?
    }

    pub async fn search(
        &self,
        input: &t3_contracts::AcpRegistrySearchInput,
    ) -> Result<t3_contracts::AcpRegistrySearchResult, RegistryError> {
        let index = self.refresh().await?;
        let environment: HashMap<String, String> = self.host_environment().into_iter().collect();
        let mut ranked = Vec::new();
        for agent in &index.agents {
            let Ok(distribution) =
                self.distribution(agent, AcpRegistryDistributionPreference::Auto)
            else {
                continue;
            };
            let Some(rank) = crate::acp_registry_search::rank(agent, input.query.as_str()) else {
                continue;
            };
            let manager = match distribution {
                SelectedDistribution::Npm(_) => Some("npm"),
                SelectedDistribution::Uv(_) => Some("uv"),
                SelectedDistribution::Binary(_) => None,
            };
            if manager.is_some_and(|manager| {
                !has_environment_path(environment.keys().map(String::as_str))
                    || executable(manager, &environment).is_none()
            }) {
                continue;
            }
            ranked.push((agent, distribution, rank, agent.name.to_lowercase()));
        }
        ranked.sort_by(|left, right| {
            left.2
                .cmp(&right.2)
                .then_with(|| crate::acp_registry_search::compare(&left.3, &right.3))
                .then_with(|| crate::acp_registry_search::compare(&left.0.id, &right.0.id))
        });
        let agents = ranked.into_iter().take(20).map(|(agent, distribution, _, _)| json!({
            "id": agent.id, "name": agent.name, "version": agent.version, "description": agent.description,
            "authors": agent.authors, "license": agent.license, "website": agent.website, "repository": agent.repository, "icon": agent.icon,
            "distribution": distribution.tag(), "integrity": if matches!(distribution, SelectedDistribution::Binary(target) if target.sha256.is_some()) { "sha256" } else { "registry" }
        })).collect::<Vec<_>>();
        serde_json::from_value(json!({"agents":agents})).map_err(|_| {
            error(
                "registry_unavailable",
                "Registry search result did not satisfy the source contract.",
            )
        })
    }
    pub async fn prepare(
        &self,
        input: &t3_contracts::AcpRegistryPrepareInput,
    ) -> Result<t3_contracts::AcpRegistryPrepareResult, RegistryError> {
        let registry = self.refresh().await?;
        let agent = registry
            .agents
            .iter()
            .find(|agent| agent.id == input.agent_id.as_str())
            .ok_or_else(|| {
                error(
                    "agent_not_found",
                    "ACP Registry does not contain the requested agent.",
                )
            })?;
        let distribution = self.distribution(agent, AcpRegistryDistributionPreference::Auto)?;
        let permit = Arc::new(self.0.install.clone().lock_owned().await);
        match distribution {
            SelectedDistribution::Binary(target) => {
                self.install_binary_locked(agent, target, permit).await?;
                let now = std::time::Instant::now();
                let mut reservations = self.0.reservations.lock().unwrap();
                reservations.retain(|_, expires| *expires > now);
                reservations.insert(agent.id.clone(), now + Duration::from_secs(30));
            }
            SelectedDistribution::Npm(target) => {
                self.install_package_locked(
                    agent,
                    target,
                    Distribution::Npm,
                    self.host_environment(),
                    permit,
                )
                .await?;
            }
            SelectedDistribution::Uv(target) => {
                self.install_package_locked(
                    agent,
                    target,
                    Distribution::Uv,
                    self.host_environment(),
                    permit,
                )
                .await?;
            }
        }
        serde_json::from_value(json!({"agentId":agent.id,"version":agent.version,"distribution":distribution.tag(),"prepared":true})).map_err(|cause|error("install_failed",cause))
    }
    pub async fn resolve(
        &self,
        settings: &AcpRegistrySettings,
        cwd: &Path,
        environment: &HashMap<String, String>,
    ) -> Result<ProcessOptions, RegistryError> {
        if settings.source == AcpRegistrySettingsSource::Local {
            return local_process(settings, cwd, environment);
        }
        let id = settings.agent_id.as_str();
        if id.is_empty() {
            return Err(error(
                "agent_not_configured",
                "ACP Registry provider requires a registry agent ID.",
            ));
        }
        let index = self.registry().await?;
        let agent = index
            .agents
            .iter()
            .find(|agent| agent.id == id)
            .ok_or_else(|| {
                error(
                    "agent_not_found",
                    format!("ACP Registry agent '{id}' was not found."),
                )
            })?;
        let distribution = self.distribution(agent, settings.distribution)?;
        let (binary, bin_directory) = if !settings.command_path.as_str().is_empty() {
            (
                executable(settings.command_path.as_str(), environment).ok_or_else(|| {
                    error(
                        "runner_unavailable",
                        "Registry command override is unavailable on this instance's PATH.",
                    )
                })?,
                None,
            )
        } else {
            match distribution {
                SelectedDistribution::Binary(target) => {
                    (self.install_binary(agent, target).await?, None)
                }
                SelectedDistribution::Npm(target) => {
                    let receipt = self
                        .install_package(agent, target, Distribution::Npm, environment)
                        .await?;
                    (
                        PathBuf::from(receipt.executable_path),
                        Some(PathBuf::from(receipt.bin_directory)),
                    )
                }
                SelectedDistribution::Uv(target) => {
                    let receipt = self
                        .install_package(agent, target, Distribution::Uv, environment)
                        .await?;
                    (
                        PathBuf::from(receipt.executable_path),
                        Some(PathBuf::from(receipt.bin_directory)),
                    )
                }
            }
        };
        let mut environment = environment.clone();
        environment.extend(
            distribution
                .environment()
                .iter()
                .map(|(key, value)| (key.clone(), value.clone())),
        );
        if let Some(bin) = bin_directory {
            environment =
                acp_registry_packages::preferred_path(&environment.into_iter().collect(), &bin)
                    .into_iter()
                    .collect();
        }
        Ok(ProcessOptions {
            binary,
            args: distribution.args().to_vec(),
            cwd: cwd.into(),
            environment,
        })
    }
    pub async fn inspection(
        &self,
        settings: &AcpRegistrySettings,
        environment: &HashMap<String, String>,
    ) -> Result<Value, RegistryError> {
        if settings.source == AcpRegistrySettingsSource::Local {
            return Ok(
                json!({"status":if settings.command_path.as_str().is_empty(){"unconfigured"}else if executable(settings.command_path.as_str(),environment).is_some(){"ready"}else{"missing_runner"},"distribution":"local","version":null}),
            );
        }
        if settings.agent_id.as_str().is_empty() {
            return Ok(json!({"status":"unconfigured"}));
        }
        let index = self.cached().await?;
        let Some(agent) = index
            .agents
            .iter()
            .find(|agent| agent.id == settings.agent_id.as_str())
        else {
            return Ok(json!({"status":"not_found","agentId":settings.agent_id}));
        };
        let distribution = match self.distribution(agent, settings.distribution) {
            Ok(distribution) => distribution,
            Err(_) => {
                return Ok(
                    json!({"status":"unsupported","agentId":agent.id,"version":agent.version}),
                );
            }
        };
        if !settings.command_path.as_str().is_empty() {
            return Ok(
                json!({"status":if executable(settings.command_path.as_str(),environment).is_some(){"ready"}else{"missing_runner"},"agentId":agent.id,"version":null,"distribution":distribution.tag()}),
            );
        }
        let documentation = agent.website.as_ref().or(agent.repository.as_ref());
        match distribution {
            SelectedDistribution::Binary(target) => {
                let _permit = self.0.install.lock().await;
                self.0.reservations.lock().unwrap().remove(&agent.id);
                let (root, path) = self.paths(agent, target)?;
                let ready = if path.exists() {
                    validate_executable(&root, &path)?;
                    true
                } else {
                    false
                };
                let mut value = json!({"status":if ready{"ready"}else{"unprepared"},"agentId":agent.id,"version":agent.version,"distribution":"binary"});
                if let Some(documentation) = documentation {
                    value["documentationUrl"] = json!(documentation);
                }
                Ok(value)
            }
            SelectedDistribution::Npm(_) | SelectedDistribution::Uv(_) => {
                let runner = if matches!(distribution, SelectedDistribution::Npm(_)) {
                    "npm"
                } else {
                    "uv"
                };
                let ready = has_environment_path(environment.keys().map(String::as_str))
                    && executable(runner, environment).is_some();
                let mut value = json!({"status":if ready{"ready"}else{"missing_runner"},"agentId":agent.id,"version":agent.version,"distribution":distribution.tag()});
                if ready {
                    if let Some(documentation) = documentation {
                        value["documentationUrl"] = json!(documentation);
                    }
                } else {
                    value["runner"] = json!(runner);
                }
                Ok(value)
            }
        }
    }
    fn paths(
        &self,
        agent: &Agent,
        target: &BinaryTarget,
    ) -> Result<(PathBuf, PathBuf), RegistryError> {
        let parts = command_parts(&target.command).ok_or_else(|| {
            error(
                "archive_invalid",
                "Registry declares an unsafe command path.",
            )
        })?;
        let platform = self.0.target.as_ref().ok_or_else(|| {
            error(
                "unsupported_platform",
                "ACP Registry does not support this platform.",
            )
        })?;
        let root = self
            .0
            .tools_dir
            .join(&agent.id)
            .join(encode_version(&agent.version))
            .join(platform);
        let path = parts
            .iter()
            .fold(root.clone(), |path, part| path.join(part));
        Ok((root, path))
    }
    async fn install_binary(
        &self,
        agent: &Agent,
        target: &BinaryTarget,
    ) -> Result<PathBuf, RegistryError> {
        let permit = Arc::new(self.0.install.clone().lock_owned().await);
        let result = self.install_binary_locked(agent, target, permit).await?;
        self.0.reservations.lock().unwrap().remove(&agent.id);
        Ok(result)
    }
    async fn install_binary_locked(
        &self,
        agent: &Agent,
        target: &BinaryTarget,
        admission: Arc<dyn Send + Sync>,
    ) -> Result<PathBuf, RegistryError> {
        let (root, path) = self.paths(agent, target)?;
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return validate_executable(&root, &path);
        }
        let parent = root.parent().unwrap();
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| error("install_failed", e))?;
        let lock_path = root.with_extension("lock");
        let lock = Arc::new(acquire_lock(&lock_path).await?);
        // Another server may have completed this exact version while we waited.
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return validate_executable(&root, &path);
        }
        let cleanup = Arc::new(Temporary {
            path: parent.join(format!(".{}-install-{}", agent.id, uuid::Uuid::new_v4())),
            _lock: lock,
            _admission: admission,
            #[cfg(test)]
            disposed: Mutex::new(None),
        });
        let temporary = &cleanup.path;
        mutate_install(cleanup.clone(), |root| std::fs::create_dir_all(root)).await?;
        let kind = ArchiveKind::from_url(&target.archive)?;
        let archive = temporary.join(kind.file_name());
        let actual = self
            .download_file(&target.archive, &archive, cleanup.clone())
            .await?;
        if let Some(expected) = &target.sha256 {
            if !actual.eq_ignore_ascii_case(expected) {
                return Err(error(
                    "checksum_mismatch",
                    "ACP Registry binary checksum mismatch.",
                ));
            }
        }
        let relative = path.strip_prefix(&root).unwrap();
        let extracted = temporary.join("extracted");
        let executable = extracted.join(relative);
        let destination = extracted.clone();
        mutate_install(cleanup.clone(), move |_| {
            std::fs::create_dir_all(destination)
        })
        .await?;
        if kind == ArchiveKind::Raw {
            let staged = executable.clone();
            mutate_install(cleanup.clone(), move |_| {
                std::fs::create_dir_all(staged.parent().unwrap())?;
                std::fs::rename(archive, &staged)
            })
            .await?;
        } else {
            #[cfg(not(test))]
            acp_registry_archives::extract(
                kind,
                &archive,
                &extracted,
                cfg!(windows),
                cleanup.clone(),
            )
            .await?;
            #[cfg(test)]
            {
                let environment = self.0.archive_environment.lock().unwrap().clone();
                acp_registry_archives::extract_with_environment(
                    kind,
                    &archive,
                    &extracted,
                    cfg!(windows),
                    cleanup.clone(),
                    environment,
                )
                .await?;
            }
        }
        if !executable.exists() {
            return Err(error(
                "archive_invalid",
                format!(
                    "ACP Registry archive for {} did not contain '{}'.",
                    agent.id, target.command
                ),
            ));
        }
        let staged = executable.clone();
        mutate_install(cleanup.clone(), move |_| {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
            }
            Ok(())
        })
        .await?;
        validate_executable(&extracted, &executable)?;
        let published_root = root.clone();
        mutate_install(cleanup.clone(), move |_| {
            if published_root.exists() {
                std::fs::remove_dir_all(&published_root)?;
            }
            std::fs::rename(extracted, published_root)
        })
        .await?;
        let result = validate_executable(&root, &path);
        result
    }
}
struct CacheTemporary(PathBuf);
impl Drop for CacheTemporary {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
struct Temporary {
    path: PathBuf,
    _lock: Arc<InstallLock>,
    _admission: Arc<dyn Send + Sync>,
    #[cfg(test)]
    disposed: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
        #[cfg(test)]
        if let Some(sender) = self.disposed.get_mut().unwrap().take() {
            let _ = sender.send(());
        }
        // The lock field drops after directory cleanup, and only after the last
        // in-flight filesystem operation relinquishes this guard.
    }
}
struct InstallArchive {
    // Struct fields drop in declaration order: close the file first, so Windows
    // can remove the directory when its final cleanup guard is released.
    file: std::fs::File,
    _cleanup: Arc<Temporary>,
}
async fn write_archive(
    file: Arc<Mutex<InstallArchive>>,
    bytes: Vec<u8>,
) -> Result<(), RegistryError> {
    tokio::task::spawn_blocking(move || file.lock().unwrap().file.write_all(&bytes))
        .await
        .map_err(|e| error("install_failed", e))?
        .map_err(|e| error("install_failed", e))
}
async fn mutate_install<F>(cleanup: Arc<Temporary>, operation: F) -> Result<(), RegistryError>
where
    F: FnOnce(&Path) -> std::io::Result<()> + Send + 'static,
{
    tokio::task::spawn_blocking(move || operation(&cleanup.path))
        .await
        .map_err(|e| error("install_failed", e))?
        .map_err(|e| error("install_failed", e))
}
fn command_parts(command: &str) -> Option<Vec<String>> {
    acp_registry_archives::command_parts(command)
}
fn executable_file(path: &Path) -> bool {
    let Ok(info) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        info.is_file() && info.permissions().mode() & 0o111 != 0
    }
    #[cfg(windows)]
    {
        info.is_file()
    }
}
fn validate_executable(root: &Path, path: &Path) -> Result<PathBuf, RegistryError> {
    let root = std::fs::canonicalize(root).map_err(|e| error("install_failed", e))?;
    let path = std::fs::canonicalize(path).map_err(|e| error("install_failed", e))?;
    if !path.starts_with(root) || !executable_file(&path) {
        return Err(error(
            "archive_invalid",
            "Cached command is not a regular executable within its install root.",
        ));
    }
    Ok(path)
}
fn has_environment_path<'a>(keys: impl Iterator<Item = &'a str>) -> bool {
    keys.into_iter()
        .any(|key| key == "PATH" || (cfg!(windows) && matches!(key, "Path" | "path")))
}

fn executable(command: &str, environment: &HashMap<String, String>) -> Option<PathBuf> {
    let expanded;
    let command = if command == "~" || command.starts_with("~/") || command.starts_with("~\\") {
        let home = environment
            .get("HOME")
            .cloned()
            .or_else(|| std::env::var("HOME").ok())
            .or_else(|| std::env::var("USERPROFILE").ok())?;
        expanded = PathBuf::from(home).join(command.get(2..).unwrap_or(""));
        expanded.to_str()?
    } else {
        command
    };
    let mut effective: indexmap::IndexMap<String, String> = std::env::vars().collect();
    effective.extend(environment.clone());
    crate::acp_registry_spawn::resolve_executable(command, &effective)
}
fn local_process(
    settings: &AcpRegistrySettings,
    cwd: &Path,
    environment: &HashMap<String, String>,
) -> Result<ProcessOptions, RegistryError> {
    if settings.command_path.as_str().is_empty() {
        return Err(error(
            "agent_not_configured",
            "Local ACP provider requires a command path.",
        ));
    }
    let binary = executable(settings.command_path.as_str(), environment).ok_or_else(|| {
        error(
            "runner_unavailable",
            "Local ACP executable is unavailable on this environment's PATH.",
        )
    })?;
    #[cfg(windows)]
    if crate::acp_registry_spawn::is_batch(&binary.to_string_lossy()) {
        return Err(error(
            "runner_unavailable",
            "Local ACP commands launch without a shell. Configure the underlying executable and pass the script path as an argument.",
        ));
    }
    Ok(ProcessOptions {
        binary,
        args: settings.command_args.clone(),
        cwd: cwd.into(),
        environment: environment.clone(),
    })
}

fn encode_version(version: &str) -> String {
    version.replace('+', "%2B")
}

struct InstallLock {
    path: PathBuf,
    #[cfg(test)]
    disposed: Option<tokio::sync::oneshot::Sender<()>>,
}
impl Drop for InstallLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        #[cfg(test)]
        if let Some(sender) = self.disposed.take() {
            let _ = sender.send(());
        }
    }
}
async fn open_owned_lock<F>(path: PathBuf, on_admitted: F) -> std::io::Result<InstallLock>
where
    F: FnOnce(&mut InstallLock) + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        // Construct inside the operation that creates the file. If the caller
        // disappears before claiming the result, Tokio drops this owned output.
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        drop(file);
        let mut lock = InstallLock {
            path,
            #[cfg(test)]
            disposed: None,
        };
        on_admitted(&mut lock);
        Ok(lock)
    })
    .await
    .map_err(std::io::Error::other)?
}
async fn acquire_lock(path: &Path) -> Result<InstallLock, RegistryError> {
    for _ in 0..300 {
        match open_owned_lock(path.to_owned(), |_| {}).await {
            Ok(lock) => return Ok(lock),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = tokio::fs::metadata(path)
                    .await
                    .ok()
                    .and_then(|info| info.modified().ok())
                    .and_then(|time| time.elapsed().ok())
                    .is_some_and(|age| age > Duration::from_secs(5 * 60));
                if stale {
                    let _ = tokio::fs::remove_file(path).await;
                    continue;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(cause) => return Err(error("install_failed", cause)),
        }
    }
    Err(error(
        "install_failed",
        format!(
            "Timed out waiting for ACP Registry install lock {}.",
            path.display()
        ),
    ))
}

#[cfg(test)]
pub(crate) fn fixture_catalog(directory: &Path, scenario: &str) -> (Catalog, AcpRegistrySettings) {
    let catalog = Catalog::with_origin(
        directory.join("cache"),
        directory.join("tools"),
        "https://fixture.invalid/registry.json".into(),
    )
    .unwrap();
    let mut binary = b"#!/usr/bin/env python3\n".to_vec();
    binary.extend_from_slice(include_bytes!("../tests/fixtures/acp-provider.py"));
    let digest = format!("{:x}", Sha256::digest(&binary));
    let target = catalog.0.target.as_ref().unwrap();
    let registry = json!({"version":"1","agents":[{"id":"devin","name":"Fixture Devin","version":"1.0.0+fixture","description":"Isolated managed process","distribution":{"binary":{target:{"archive":"https://fixture.invalid/agent.py","cmd":"bin/agent","sha256":digest,"args":["2",scenario],"env":{"ACP_FIXTURE_RECIPE":"recipe"}}}}}]});
    catalog.0.assets.lock().unwrap().insert(
        catalog.0.url.clone(),
        serde_json::to_vec(&registry).unwrap(),
    );
    catalog
        .0
        .assets
        .lock()
        .unwrap()
        .insert("https://fixture.invalid/agent.py".into(), binary);
    let settings = serde_json::from_value(json!({"source":"registry","agentId":"devin","distribution":"binary","commandArgs":["must-not-be-forwarded"]})).unwrap();
    (catalog, settings)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    fn package_fixture(
        directory: &Path,
        uv: bool,
    ) -> (Catalog, AcpRegistrySettings, HashMap<String, String>) {
        use std::os::unix::fs::PermissionsExt;
        let (catalog, _) = fixture_catalog(directory, "normal");
        let binary = catalog.0.assets.lock().unwrap()["https://fixture.invalid/agent.py"].clone();
        let source = directory.join("fixture-provider.py");
        std::fs::write(&source, binary).unwrap();
        let commands = directory.join("commands");
        std::fs::create_dir(&commands).unwrap();
        let manager = commands.join(if uv { "uv" } else { "npm" });
        let mut code = b"#!/usr/bin/env python3\n".to_vec();
        code.extend_from_slice(include_bytes!("../tests/fixtures/acp-registry-manager.py"));
        std::fs::write(&manager, code).unwrap();
        std::fs::set_permissions(&manager, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut environment = std::env::vars().collect::<IndexMap<_, _>>();
        environment.insert(
            "PATH".into(),
            format!(
                "{}:{}",
                commands.display(),
                environment.get("PATH").unwrap()
            ),
        );
        environment.insert(
            "REGISTRY_PROVIDER_SOURCE".into(),
            source.to_str().unwrap().into(),
        );
        environment.insert(
            "REGISTRY_MANAGER_LOG".into(),
            directory.join("manager.log").to_str().unwrap().into(),
        );
        *catalog.0.archive_environment.lock().unwrap() = Some(environment.clone());
        let mut assets = catalog.0.assets.lock().unwrap();
        let mut registry: Value = serde_json::from_slice(&assets[&catalog.0.url]).unwrap();
        registry["agents"][0]["distribution"] = json!({if uv{"uvx"}else{"npx"}:{"package":if uv{"fixture-acp==1.2.3"}else{"fixture-acp@1.2.3"},"args":["2","normal"],"env":{"ACP_FIXTURE_RECIPE":"recipe"}}});
        assets.insert(
            catalog.0.url.clone(),
            serde_json::to_vec(&registry).unwrap(),
        );
        drop(assets);
        let settings=serde_json::from_value(json!({"source":"registry","agentId":"devin","distribution":"auto","commandArgs":["must-not-be-forwarded"]})).unwrap();
        (catalog, settings, environment.into_iter().collect())
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn catalog_prepares_npm_and_uv_then_launches_managed_providers_with_recipe_and_path() {
        tokio::time::timeout(Duration::from_secs(20), async {
            for uv in [false, true] {
                let directory = tempfile::tempdir().unwrap();
                let (catalog, settings, environment) = package_fixture(directory.path(), uv);
                assert!(catalog.inspection(&settings, &environment).await.is_err());
                assert_eq!(catalog.0.downloads.load(Ordering::Relaxed), 0);
                catalog.refresh().await.unwrap();
                assert_eq!(
                    catalog.inspection(&settings, &environment).await.unwrap()["status"],
                    "ready"
                );
                assert!(
                    !catalog.0.tools_dir.exists(),
                    "cold package inspection does not install"
                );
                let prepared = catalog
                    .prepare(&serde_json::from_value(json!({"agentId":"devin"})).unwrap())
                    .await
                    .unwrap();
                assert_eq!(
                    serde_json::to_value(prepared).unwrap()["distribution"],
                    if uv { "uvx" } else { "npx" }
                );
                let process = catalog
                    .resolve(&settings, directory.path(), &environment)
                    .await
                    .unwrap();
                assert_eq!(process.args, ["2", "normal"]);
                assert_eq!(process.environment["ACP_FIXTURE_RECIPE"], "recipe");
                assert_eq!(
                    process.environment["PATH"].split(':').next().unwrap(),
                    process.binary.parent().unwrap().to_str().unwrap()
                );
                let peer = crate::acp_peer::ProcessPeer::spawn(process).unwrap();
                let client = t3_acp::Client::new(Arc::new(peer.clone()), Duration::from_secs(3));
                let initialized = client
                    .initialize(
                        json!({"protocolVersion":2,"clientInfo":{"name":"test","version":"1"}}),
                    )
                    .await
                    .unwrap();
                client.shutdown();
                peer.shutdown().await;
                assert_eq!(initialized["agentInfo"]["name"], "fixture");
                let calls = std::fs::read_to_string(directory.path().join("manager.log")).unwrap();
                let installs = calls
                    .lines()
                    .map(|line| serde_json::from_str::<Value>(line).unwrap())
                    .filter(|row| row["args"][if uv { 1 } else { 0 }] == "install")
                    .count();
                assert_eq!(installs, 1);
            }
        })
        .await
        .unwrap();
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn canceled_catalog_package_manager_retains_real_admission_until_reap_then_retry_installs()
     {
        tokio::time::timeout(Duration::from_secs(20), async {
            for uv in [false, true] {
                let directory = tempfile::tempdir().unwrap();
                let (catalog, settings, mut environment) = package_fixture(directory.path(), uv);
                let socket = directory.path().join("started.sock");
                let receiver = tokio::net::UnixDatagram::bind(&socket).unwrap();
                environment.insert("REGISTRY_MANAGER_HOLD".into(), "1".into());
                environment.insert(
                    "REGISTRY_COMMAND_STARTED".into(),
                    socket.to_str().unwrap().into(),
                );
                let pending_catalog = catalog.clone();
                let pending_settings = settings.clone();
                let pending_environment = environment.clone();
                let cwd = directory.path().to_owned();
                let pending = tokio::spawn(async move {
                    pending_catalog
                        .resolve(&pending_settings, &cwd, &pending_environment)
                        .await
                });
                let mut bytes = [0; 64];
                let count = receiver.recv(&mut bytes).await.unwrap();
                let pid = std::str::from_utf8(&bytes[..count])
                    .unwrap()
                    .parse::<i32>()
                    .unwrap();
                assert!(catalog.0.install.try_lock().is_err());
                pending.abort();
                assert!(pending.await.unwrap_err().is_cancelled());
                let admission = catalog.0.install.clone().lock_owned().await;
                assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::ESRCH)
                );
                assert!(
                    !catalog.0.cache_dir.join("package-installs").exists(),
                    "cancelled manager did not publish a receipt"
                );
                environment.remove("REGISTRY_MANAGER_HOLD");
                drop(admission);
                let process = catalog
                    .resolve(&settings, directory.path(), &environment)
                    .await
                    .unwrap();
                assert!(process.binary.is_file());
                assert_eq!(
                    std::fs::read_dir(catalog.0.cache_dir.join("package-installs"))
                        .unwrap()
                        .count(),
                    1
                );
            }
        })
        .await
        .unwrap();
    }
    async fn archive_fixture(directory: &Path, kind: &str) -> (Catalog, AcpRegistrySettings) {
        let (catalog, settings) = fixture_catalog(directory, "normal");
        let binary = catalog.0.assets.lock().unwrap()["https://fixture.invalid/agent.py"].clone();
        let raw = directory.join("fixture-agent.py");
        std::fs::write(&raw, binary).unwrap();
        let extension = match kind {
            "gz" => "tar.gz",
            "bz2" => "tar.bz2",
            _ => "zip",
        };
        let archive = directory.join(format!("fixture.{extension}"));
        let script = "import io,sys,tarfile,zipfile\ndata=open(sys.argv[1],'rb').read()\nif sys.argv[3]=='zip':\n with zipfile.ZipFile(sys.argv[2],'w') as archive: archive.writestr('bin/agent',data)\nelse:\n with tarfile.open(sys.argv[2],'w:'+sys.argv[3]) as archive:\n  entry=tarfile.TarInfo('bin/agent');entry.size=len(data);archive.addfile(entry,io.BytesIO(data))";
        let mut build = crate::acp_registry_commands::RunningCommand::start(
            crate::acp_registry_commands::CommandOptions {
                command: "python3".into(),
                arguments: vec![
                    "-c".into(),
                    script.into(),
                    raw.to_str().unwrap().into(),
                    archive.to_str().unwrap().into(),
                    kind.into(),
                ],
                cwd: None,
                environment: None,
                timeout: None,
                truncated_output_reason: "archive_invalid",
            },
            Arc::new(()),
        )
        .unwrap();
        build.wait().await.unwrap();
        let bytes = std::fs::read(archive).unwrap();
        let url = format!("https://fixture.invalid/agent.{extension}");
        let mut assets = catalog.0.assets.lock().unwrap();
        let mut registry: Value = serde_json::from_slice(&assets[&catalog.0.url]).unwrap();
        let recipe = &mut registry["agents"][0]["distribution"]["binary"]
            [catalog.0.target.as_ref().unwrap()];
        recipe["archive"] = json!(url);
        recipe["sha256"] = json!(format!("{:x}", Sha256::digest(&bytes)));
        assets.insert(
            catalog.0.url.clone(),
            serde_json::to_vec(&registry).unwrap(),
        );
        assets.insert(url, bytes);
        drop(assets);
        (catalog, settings)
    }
    #[tokio::test]
    async fn catalog_prepares_archives_then_resolves_and_initializes_the_managed_provider() {
        tokio::time::timeout(Duration::from_secs(20), async {
            for kind in ["gz", "bz2", "zip"] {
                let directory = tempfile::tempdir().unwrap();
                let (catalog, settings) = archive_fixture(directory.path(), kind).await;
                let prepared = catalog
                    .prepare(&serde_json::from_value(json!({"agentId":"devin"})).unwrap())
                    .await
                    .unwrap();
                assert_eq!(
                    serde_json::to_value(prepared).unwrap()["distribution"],
                    "binary"
                );
                let environment = std::env::vars().collect();
                assert_eq!(
                    catalog.inspection(&settings, &environment).await.unwrap()["status"],
                    "ready"
                );
                let process = catalog
                    .resolve(&settings, directory.path(), &environment)
                    .await
                    .unwrap();
                let peer = crate::acp_peer::ProcessPeer::spawn(process).unwrap();
                let client = t3_acp::Client::new(Arc::new(peer.clone()), Duration::from_secs(3));
                let response = client
                    .initialize(
                        json!({"protocolVersion":2,"clientInfo":{"name":"test","version":"1"}}),
                    )
                    .await
                    .unwrap();
                client.shutdown();
                peer.shutdown().await;
                assert_eq!(response["agentInfo"]["name"], "fixture");
            }
        })
        .await
        .unwrap();
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn canceled_catalog_extractor_holds_real_admission_until_reap_and_allows_clean_retry() {
        tokio::time::timeout(Duration::from_secs(15),async {
            use std::os::unix::fs::PermissionsExt;
            let directory=tempfile::tempdir().unwrap();
            let (catalog,settings)=archive_fixture(directory.path(),"gz").await;
            let commands=directory.path().join("commands");std::fs::create_dir(&commands).unwrap();
            let wrapper=commands.join("tar");
            std::fs::write(&wrapper,"#!/usr/bin/env python3\nimport os,signal,socket\ns=socket.socket(socket.AF_UNIX,socket.SOCK_DGRAM)\ns.sendto(str(os.getpid()).encode(),os.environ['REGISTRY_COMMAND_STARTED'])\nwhile True: signal.pause()\n").unwrap();
            std::fs::set_permissions(&wrapper,std::fs::Permissions::from_mode(0o755)).unwrap();
            let socket=directory.path().join("started.sock");
            let receiver=tokio::net::UnixDatagram::bind(&socket).unwrap();
            let mut environment=std::env::vars().collect::<IndexMap<_,_>>();
            environment.insert("PATH".into(),format!("{}:{}",commands.display(),environment.get("PATH").unwrap()));
            environment.insert("REGISTRY_COMMAND_STARTED".into(),socket.to_str().unwrap().into());
            *catalog.0.archive_environment.lock().unwrap()=Some(environment);
            let pending_catalog=catalog.clone();let pending_settings=settings.clone();let cwd=directory.path().to_owned();
            let pending=tokio::spawn(async move{pending_catalog.resolve(&pending_settings,&cwd,&std::env::vars().collect()).await});
            let mut buffer=[0;64];let count=receiver.recv(&mut buffer).await.unwrap();
            let pid=std::str::from_utf8(&buffer[..count]).unwrap().parse::<i32>().unwrap();
            assert!(catalog.0.install.try_lock().is_err());
            pending.abort();assert!(pending.await.unwrap_err().is_cancelled());
            // This real catalog permit is released only after the extractor's
            // cleanup guard has removed its temporary tree and OS lock.
            let admission=catalog.0.install.clone().lock_owned().await;
            assert_eq!(unsafe{libc::kill(pid,0)},-1);
            assert_eq!(std::io::Error::last_os_error().raw_os_error(),Some(libc::ESRCH));
            let index=catalog.cached().await.unwrap();let agent=&index.agents[0];let target=catalog.binary(agent,settings.distribution).unwrap();
            let (root,_)=catalog.paths(agent,target).unwrap();assert!(!root.exists());assert!(!root.with_extension("lock").exists());
            assert!(std::fs::read_dir(root.parent().unwrap()).unwrap().all(|entry|!entry.unwrap().file_name().to_string_lossy().contains("install-")));
            *catalog.0.archive_environment.lock().unwrap()=None;
            drop(admission);
            let retry=catalog.resolve(&settings,directory.path(),&std::env::vars().collect()).await.unwrap();
            assert!(retry.binary.is_file());
        }).await.unwrap();
    }
    #[tokio::test]
    async fn cancelled_lock_admission_drops_unclaimed_owned_guard_after_os_create_finishes() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("install.lock");
            let task_path = path.clone();
            let (admitted, admission) = tokio::sync::oneshot::channel();
            let (removed, removal) = tokio::sync::oneshot::channel();
            let (release, released) = std::sync::mpsc::channel();
            let caller = tokio::spawn(async move {
                open_owned_lock(task_path, move |guard| {
                    guard.disposed = Some(removed);
                    admitted.send(()).unwrap();
                    // This is the actual created lock, deliberately held before
                    // the blocking operation transfers its output to the caller.
                    released.recv().unwrap();
                })
                .await
            });
            admission.await.unwrap();
            assert!(path.exists());
            caller.abort();
            assert!(matches!(caller.await,Err(error) if error.is_cancelled()));
            assert!(
                path.exists(),
                "pending OS operation still owns the created lock"
            );
            release.send(()).unwrap();
            removal.await.unwrap();
            assert!(
                !path.exists(),
                "unclaimed output must not orphan the install lock"
            );
        })
        .await
        .expect("Owned lock-admission milestones stalled");
    }
    #[tokio::test]
    async fn cancelled_directory_mutation_retains_temp_cleanup_and_lock_until_last_operation_finishes()
     {
        tokio::time::timeout(Duration::from_secs(5), async {
            let directory = tempfile::tempdir().unwrap();
            let lock_path = directory.path().join("install.lock");
            let path = directory.path().join("temporary");
            let (lock_removed, lock_removal) = tokio::sync::oneshot::channel();
            let mut lock = open_owned_lock(lock_path.clone(), |_| {}).await.unwrap();
            lock.disposed = Some(lock_removed);
            let (removed, removal) = tokio::sync::oneshot::channel();
            let cleanup = Arc::new(Temporary {
                path: path.clone(),
                _lock: Arc::new(lock),
                _admission: Arc::new(()),
                disposed: Mutex::new(Some(removed)),
            });
            let (created, creation) = tokio::sync::oneshot::channel();
            let (release, released) = std::sync::mpsc::channel();
            let caller = tokio::spawn(async move {
                mutate_install(cleanup, move |root| {
                    std::fs::create_dir_all(root)?;
                    created.send(()).unwrap();
                    released.recv().unwrap();
                    // A detached mutation can still finish after cancellation;
                    // its guard must outlive that final filesystem side effect.
                    std::fs::create_dir_all(root.join("late-child"))
                })
                .await
            });
            creation.await.unwrap();
            caller.abort();
            assert!(matches!(caller.await,Err(error) if error.is_cancelled()));
            assert!(path.exists());
            assert!(lock_path.exists());
            release.send(()).unwrap();
            removal.await.unwrap();
            lock_removal.await.unwrap();
            assert!(!path.exists());
            assert!(!lock_path.exists());
        })
        .await
        .expect("Owned directory-mutation milestones stalled");
    }
    #[test]
    fn pinned_packages_match_original_ecmascript_whitespace_casefold_and_end_anchors() {
        for (row, line) in include_str!("../tests/fixtures/acp-registry-packages.jsonl")
            .lines()
            .enumerate()
        {
            let fixture: Value = serde_json::from_str(line).unwrap();
            let decoded = package(
                &json!({"package":fixture["input"]}),
                fixture["npx"].as_bool().unwrap(),
            );
            assert_eq!(
                decoded.is_ok(),
                fixture["ok"].as_bool().unwrap(),
                "source case {row}: {fixture}"
            );
        }
    }
    #[tokio::test]
    async fn unchanged_version_refresh_coalesces_and_invalid_download_falls_back_to_disk_not_memory()
     {
        let directory = tempfile::tempdir().unwrap();
        let (catalog, _) = fixture_catalog(directory.path(), "normal");
        catalog.refresh().await.unwrap();
        assert_eq!(catalog.0.downloads.load(Ordering::Relaxed), 1);
        let observed = catalog.0.revision.load(Ordering::Acquire);
        let (first, second) = tokio::join!(
            catalog.refresh_observed(observed),
            catalog.refresh_observed(observed)
        );
        assert_eq!(first.unwrap().version, "1");
        assert_eq!(second.unwrap().version, "1");
        assert_eq!(
            catalog.0.downloads.load(Ordering::Relaxed),
            2,
            "unchanged content version must still share one refresh"
        );
        let disk = json!({"version":"disk-version","agents":[]});
        tokio::fs::write(
            catalog.0.cache_dir.join("registry.json"),
            serde_json::to_vec(&disk).unwrap(),
        )
        .await
        .unwrap();
        catalog
            .0
            .assets
            .lock()
            .unwrap()
            .insert(catalog.0.url.clone(), b"not-json".to_vec());
        assert_eq!(catalog.refresh().await.unwrap().version, "disk-version");
        tokio::fs::remove_file(catalog.0.cache_dir.join("registry.json"))
            .await
            .unwrap();
        assert!(
            catalog.refresh().await.is_err(),
            "in-memory state is not the source disk fallback"
        );
    }
    #[tokio::test]
    async fn cold_inspection_never_fetches_and_raw_install_verifies_digest_paths_and_recipe() {
        let directory = tempfile::tempdir().unwrap();
        let (catalog, settings) = fixture_catalog(directory.path(), "normal");
        let environment = HashMap::from([("ACP_FIXTURE_RECIPE".into(), "instance".into())]);
        assert!(catalog.inspection(&settings, &environment).await.is_err());
        assert_eq!(catalog.0.downloads.load(Ordering::Relaxed), 0);
        let process = catalog
            .resolve(&settings, directory.path(), &environment)
            .await
            .unwrap();
        assert!(process.binary.to_string_lossy().contains("1.0.0%2Bfixture"));
        assert_eq!(process.args, ["2", "normal"]);
        assert_eq!(process.environment["ACP_FIXTURE_RECIPE"], "recipe");
        assert_eq!(
            catalog.inspection(&settings, &environment).await.unwrap()["status"],
            "ready"
        );
        let index = catalog.cached().await.unwrap();
        let agent = &index.agents[0];
        let mut target = catalog
            .binary(agent, settings.distribution)
            .unwrap()
            .clone();
        target.command = "../escape".into();
        assert_eq!(
            catalog
                .install_binary(agent, &target)
                .await
                .unwrap_err()
                .reason,
            "archive_invalid"
        );
        target.command = "bin/other".into();
        target.sha256 = Some("0".repeat(64));
        // A different version forces the checksum path instead of using the prepared executable.
        let mut different = agent.clone();
        different.version = "2".into();
        assert_eq!(
            catalog
                .install_binary(&different, &target)
                .await
                .unwrap_err()
                .reason,
            "checksum_mismatch"
        );
        let (root, _) = catalog.paths(&different, &target).unwrap();
        assert!(!root.exists());
        assert!(!root.with_extension("lock").exists());
        assert!(
            std::fs::read_dir(root.parent().unwrap())
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains("install-"))
        );
    }
    async fn uninstall_settings(
        directory: &Path,
        settings: Value,
    ) -> crate::server_settings::SettingsService {
        std::fs::create_dir_all(directory).unwrap();
        std::fs::write(directory.join("settings.json"), settings.to_string()).unwrap();
        let secrets =
            crate::server_secret_store::ServerSecretStore::open(directory.join("secrets")).unwrap();
        let mut options =
            crate::server_settings::SettingsOptions::file(directory.join("settings.json"), secrets);
        options.watch = false;
        crate::server_settings::SettingsService::start(options)
            .await
            .unwrap()
    }
    #[tokio::test]
    async fn catalog_search_refreshes_filters_unavailable_distributions_and_caps_sorted_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let (catalog, _) = fixture_catalog(directory.path(), "normal");
        *catalog.0.archive_environment.lock().unwrap() =
            Some(IndexMap::from([("PATH".into(), String::new())]));
        let target = catalog.0.target.clone().unwrap();
        let mut agents = (0..30).rev().map(|id| json!({"id":format!("item-{id:02}"),"name":format!("Name {id:02}"),"version":"1","description":"fixture","authors":["Author"],"license":"MIT","website":"https://example.invalid/","distribution":{"binary":{&target:{"archive":"https://fixture.invalid/a","cmd":"agent","sha256":"a".repeat(64)}}}})).collect::<Vec<_>>();
        let mut tie = agents.last().unwrap().clone();
        tie["id"] = json!("item-00-extra");
        tie["distribution"]["binary"][&target]
            .as_object_mut()
            .unwrap()
            .remove("sha256");
        agents.push(tie);
        agents.push(json!({"id":"npm-only","name":"Npm","version":"1","description":"fixture","distribution":{"npx":{"package":"fixture@1.0.0"}}}));
        agents.push(json!({"id":"unsupported","name":"Unsupported","version":"1","description":"fixture","distribution":{"binary":{"other-platform":{"archive":"https://fixture.invalid/a","cmd":"agent"}}}}));
        agents.push(json!({"id":"INVALID","name":"Invalid","version":"1","description":"discard","distribution":{}}));
        catalog.0.assets.lock().unwrap().insert(
            catalog.0.url.clone(),
            serde_json::to_vec(&json!({"version":"1","agents":agents})).unwrap(),
        );
        let input = serde_json::from_value(json!({"query":""})).unwrap();
        let output = serde_json::to_value(catalog.search(&input).await.unwrap()).unwrap();
        let results = output["agents"].as_array().unwrap();
        assert_eq!(results.len(), 20);
        assert_eq!(results[0]["id"], "item-00");
        assert_eq!(
            results[1]["id"], "item-00-extra",
            "equal lowercase names use ID order"
        );
        assert_eq!(results[1]["integrity"], "registry");
        assert_eq!(results[19]["id"], "item-18");
        assert_eq!(results[0]["integrity"], "sha256");
        assert_eq!(results[0]["license"], "MIT");
        assert_eq!(results[0]["repository"], Value::Null);
        assert_eq!(results[0]["authors"], json!(["Author"]));
        let exact = serde_json::from_value(json!({"query":"Name 29"})).unwrap();
        assert_eq!(
            serde_json::to_value(catalog.search(&exact).await.unwrap()).unwrap()["agents"][0]["id"],
            "item-29"
        );
        assert_eq!(
            catalog.0.downloads.load(Ordering::Acquire),
            2,
            "search refreshes even with a populated cache"
        );
        assert!(
            !directory.path().join("tools").exists(),
            "search cannot install anything"
        );
        let (mut unsupported, _) =
            fixture_catalog(&directory.path().join("unsupported-host"), "normal");
        Arc::get_mut(&mut unsupported.0).unwrap().target = None;
        assert!(
            unsupported
                .search(&input)
                .await
                .unwrap()
                .agents
                .0
                .is_empty()
        );
    }
    #[tokio::test]
    async fn uninstall_protects_prepared_and_disabled_referenced_agents_then_removes_only_binaries()
    {
        let directory = tempfile::tempdir().unwrap();
        let (catalog, _) = fixture_catalog(directory.path(), "normal");
        let input = serde_json::from_value(json!({"agentId":"devin"})).unwrap();
        catalog
            .prepare(&serde_json::from_value(json!({"agentId":"devin"})).unwrap())
            .await
            .unwrap();
        let before_uninstall = catalog.0.downloads.load(Ordering::Acquire);
        let root = directory.path().join("tools/devin/1.0.0%2Bfixture");
        for package in ["npm", "python"] {
            std::fs::create_dir(root.join(package)).unwrap();
            std::fs::write(root.join(package).join("keep"), "package").unwrap();
        }
        let empty = uninstall_settings(&directory.path().join("empty"), json!({})).await;
        assert!(
            !catalog
                .uninstall_managed_binary(&input, &empty)
                .await
                .unwrap()
                .removed
        );
        let referenced = uninstall_settings(&directory.path().join("referenced"),json!({"providerInstances":{"ref":{"driver":"acpRegistry","enabled":false,"config":{"agentId":" devin "}}}})).await;
        assert!(
            !catalog
                .uninstall_managed_binary(&input, &referenced)
                .await
                .unwrap()
                .removed,
            "disabled registry instances still hold a reference"
        );
        assert!(
            !catalog.0.reservations.lock().unwrap().contains_key("devin"),
            "a reference consumes its prepared reservation"
        );
        assert!(
            catalog
                .uninstall_managed_binary(&input, &empty)
                .await
                .unwrap()
                .removed
        );
        assert!(root.join("npm/keep").is_file());
        assert!(root.join("python/keep").is_file());
        assert!(!root.join(catalog.0.target.as_ref().unwrap()).exists());
        assert!(
            !catalog
                .uninstall_managed_binary(&input, &empty)
                .await
                .unwrap()
                .removed
        );
        assert_eq!(
            catalog.0.downloads.load(Ordering::Acquire),
            before_uninstall,
            "uninstall never refreshes the registry"
        );
        empty.shutdown().await;
        referenced.shutdown().await;
    }
    #[tokio::test]
    async fn canceled_native_uninstall_retains_settings_and_install_admission_through_real_deletion()
     {
        tokio::time::timeout(Duration::from_secs(10),async {
            let directory = tempfile::tempdir().unwrap();
            let (catalog,_) = fixture_catalog(directory.path(),"normal");
            let root = directory.path().join("tools/devin/1/linux-x86_64");
            std::fs::create_dir_all(&root).unwrap();std::fs::write(root.join("agent"),"owned").unwrap();
            let service = uninstall_settings(&directory.path().join("settings"),json!({})).await;
            let (entered,admitted)=tokio::sync::oneshot::channel();
            let (release,released)=std::sync::mpsc::channel();
            *catalog.0.delete_gate.lock().unwrap()=Some((entered,released));
            let caller=tokio::spawn({let catalog=catalog.clone();let service=service.clone();async move {catalog.uninstall_managed_binary(&serde_json::from_value(json!({"agentId":"devin"})).unwrap(),&service).await}});
            admitted.await.unwrap();caller.abort();assert!(caller.await.unwrap_err().is_cancelled());
            assert!(catalog.0.install.try_lock().is_err(),"the actual native delete job retains install ownership after caller cancellation");
            let patch=serde_json::from_value(json!({"providerInstances":{"new-ref":{"driver":"acpRegistry","config":{"agentId":"devin"}}}})).unwrap();
            let mut update=Box::pin(service.update(patch));
            assert!(futures_util::poll!(&mut update).is_pending(),"new settings reference cannot be published before deletion completes");
            assert!(!service.snapshot().await.unwrap().provider_instances.contains_key(&"new-ref".parse().unwrap()));
            assert!(root.join("agent").is_file());
            release.send(()).unwrap();
            let updated=update.await.unwrap();assert!(updated.provider_instances.contains_key(&"new-ref".parse().unwrap()));
            let _admission=catalog.0.install.lock().await;
            assert!(!directory.path().join("tools/devin").exists(),"the owned blocking operation really deleted the binary before either admission was released");
            service.shutdown().await;
        }).await.expect("Uninstall ownership milestones stalled");
    }
}
