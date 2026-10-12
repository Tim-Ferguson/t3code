//! Managed Codex installation (CodexInstallation.ts and providerInstallation.ts): where it lives, how a
//! download is verified, the progress events clients watch, cancellation and removal.
//!
//! A managed install lives in `<tools>/codex`: `<version>/` holds the extracted official package and
//! `.install-complete.json`, `active.json` names the version in use. A supported `codex` already on the
//! user's PATH is preferred over it. The package is downloaded from the one official release host named
//! in the table below, checked against its pinned size and SHA-256, listed and validated before anything
//! is extracted, extracted into a staging directory and only run (`--version`) after all of that.
use crate::{
    provider_manage::Management,
    provider_maintenance::{self as maintenance, Platform},
    provider_registry::ProviderRegistry,
};
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, atomic::{AtomicUsize, Ordering}},
    time::Duration,
};
use t3_contracts::{ProviderInstallCancelInput, ProviderInstanceId, ProviderSetupError, ProviderSetupErrorTag};
use tokio::sync::broadcast;

/// The only place a managed Codex package is downloaded from.
pub const OFFICIAL_RELEASE_PREFIX: &str = "https://github.com/openai/codex/releases/download/";

#[derive(Clone, Debug, PartialEq)]
pub struct ReleaseAsset {
    pub version: String,
    pub target: String,
    pub url: String,
    pub sha256: String,
    pub archive_bytes: u64,
}
impl ReleaseAsset {
    pub fn to_json(&self) -> Value {
        json!({"version":self.version,"target":self.target,"url":self.url,"sha256":self.sha256,"archiveBytes":self.archive_bytes})
    }
}
/// The official complete packages, which retain the code-mode, search and resource companions.
/// `platform` and `arch` are Node's names (`darwin`, `x64`, ...).
pub fn release_asset(platform: &str, arch: &str) -> Option<ReleaseAsset> {
    let (target, sha256, archive_bytes) = match (platform, arch) {
        ("darwin", "arm64") => ("aarch64-apple-darwin", "fea42f9625091f011e38f059da974d52e57ba31831648bb1c7f0b1a385fde547", 127394863),
        ("darwin", "x64") => ("x86_64-apple-darwin", "618dbcd55419fa041871f777a14b107ceb3fe2d339ef81e21e6ab5374420dc71", 138670455),
        ("linux", "arm64") => ("aarch64-unknown-linux-musl", "fdd47ed6aade0360796fd3f6f95a45096f327c15e19e8c7339f9dc5633041786", 136933361),
        ("linux", "x64") => ("x86_64-unknown-linux-musl", "8b711520beddf385467b8da4d2c93736637c6ba1e46811cf0d8606b7c490b6f6", 145976992),
        ("win32", "arm64") => ("aarch64-pc-windows-msvc", "85994caecdc7609c49fd585c1cdb5677fa9d0acbf789650cff23a13afc9505db", 142037952),
        ("win32", "x64") => ("x86_64-pc-windows-msvc", "a2e017db9807e6a2269a26fea0e1d9546469cef4d472a33016bc9f3ad7d3b733", 153839991),
        _ => return None,
    };
    Some(ReleaseAsset { version: "0.156.1".into(), target: target.into(), url: format!("{OFFICIAL_RELEASE_PREFIX}rust-v0.156.1/codex-package-{target}.tar.gz"), sha256: sha256.into(), archive_bytes })
}
fn host_names() -> (&'static str, &'static str) {
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
    (platform, arch)
}

#[derive(Clone, Debug, PartialEq)]
pub struct InstallError {
    pub operation: String,
    pub detail: String,
}
impl InstallError {
    fn new(operation: &str, detail: &str) -> Self {
        Self { operation: operation.into(), detail: detail.into() }
    }
    /// A failure with no message of its own (I/O, decoding, transport): the caller's default replaces it, so
    /// paths, URLs and other detail never reach a client.
    fn raw() -> Self {
        Self { operation: String::new(), detail: String::new() }
    }
    fn or(self, operation: &str, detail: &str) -> Self {
        if self.operation.is_empty() { Self::new(operation, detail) } else { self }
    }
}
impl From<std::io::Error> for InstallError {
    fn from(_: std::io::Error) -> Self {
        Self::raw()
    }
}
impl From<serde_json::Error> for InstallError {
    fn from(_: serde_json::Error) -> Self {
        Self::raw()
    }
}
impl From<reqwest::Error> for InstallError {
    fn from(_: reqwest::Error) -> Self {
        Self::raw()
    }
}
type R<T> = Result<T, InstallError>;

fn version_pattern() -> &'static Regex {
    static VERSION: OnceLock<Regex> = OnceLock::new();
    VERSION.get_or_init(|| Regex::new(r"^[0-9]+\.[0-9]+\.[0-9]+(?:-[a-zA-Z0-9.-]+)?$").unwrap())
}
/// `Version`: a release directory name, so also the guard against `..` and separators.
pub fn valid_version(version: &str) -> bool {
    version_pattern().is_match(version)
}
/// The archive listing rule: bounded, plain files and directories only, no escaping or odd names.
pub fn unsafe_archive(entries: &[&str], types: &[&str]) -> bool {
    entries.len() > 10000
        || entries.len() != types.len()
        || types.iter().any(|line| !matches!(line.chars().next(), Some('-' | 'd')))
        || entries.iter().any(|entry| {
            let parts: Vec<&str> = entry.strip_suffix('/').unwrap_or(entry).split('/').collect();
            entry.is_empty() || entry.contains('\\') || entry.starts_with('/') || parts.iter().any(|part| matches!(*part, ".." | "." | "") || part.contains(':'))
        })
}

#[derive(Deserialize)]
struct ActiveRelease {
    version: String,
}
#[derive(Deserialize)]
struct InstalledRelease {
    version: String,
    target: String,
    sha256: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PackageManifest {
    layout_version: u8,
    version: String,
    target: String,
    entrypoint: String,
}

#[derive(Clone, Debug)]
pub struct Executable {
    pub path: PathBuf,
    pub source: &'static str,
    pub version: String,
}

pub struct InstallOptions {
    /// `<tools>/codex`.
    pub managed_directory: PathBuf,
    /// `None` for a platform the vendor publishes nothing for. `trusted` is set only by code that supplies
    /// its own asset (tests), which waives the official-host rule.
    pub asset: Option<ReleaseAsset>,
    pub trusted_asset: bool,
    /// Where a `codex` already installed is looked for; the process `PATH` when unset.
    pub path: Option<String>,
    pub platform: &'static str,
    pub arch: &'static str,
}

struct Inner {
    managed: PathBuf,
    asset: Option<ReleaseAsset>,
    trusted: bool,
    path: Option<String>,
    platform: &'static str,
    arch: &'static str,
    gate: tokio::sync::Mutex<()>,
    state: Mutex<Value>,
    changes: broadcast::Sender<Arc<Value>>,
    running: Mutex<Option<(String, tokio::task::JoinHandle<()>)>>,
    local: Mutex<HashMap<String, (String, Option<Executable>)>>,
    /// The leases `acquire` has given out and not yet seen released.
    leases: AtomicUsize,
}
#[derive(Clone)]
pub struct CodexInstallation(Arc<Inner>);

/// What `acquire` gives with the executable (`CodexInstallation.acquire`'s scope): a Codex process, or a sign-in that runs on the
/// runtime, holds it for as long as it lives, and the runtime is not removed while any is held. Cloning shares the one lease.
#[derive(Clone)]
pub struct Lease(Arc<LeaseGuard>);
struct LeaseGuard(Arc<Inner>);
impl Drop for LeaseGuard {
    fn drop(&mut self) {
        self.0.leases.fetch_sub(1, Ordering::SeqCst);
    }
}
impl std::fmt::Debug for Lease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Lease")
    }
}

fn is_running(state: &Value) -> bool {
    matches!(state["phase"].as_str(), Some("downloading" | "extracting" | "verifying"))
}
/// Removes a staging directory when the installation that owns it ends, however it ends (including cancellation).
struct Staging(PathBuf);
impl Staging {
    /// A fresh private directory beside what it will become part of, so that a rename into place stays on one filesystem.
    fn create(parent: &Path, prefix: &str) -> std::io::Result<Self> {
        let directory = parent.join(format!("{prefix}{}", uuid::Uuid::new_v4().simple()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&directory)?;
        Ok(Self(directory))
    }
}
impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
async fn run_capture(program: &Path, args: &[&str]) -> R<String> {
    use tokio::io::AsyncReadExt;
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let (mut stdout, mut stderr) = (child.stdout.take().expect("piped"), child.stderr.take().expect("piped"));
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let (a, b, status) = tokio::join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err), child.wait());
    a?;
    b?;
    if !status?.success() {
        return Err(InstallError::new("verify", "Could not unpack or start the downloaded Codex package."));
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

impl CodexInstallation {
    /// Builds the service and reads what is already installed, as the source does at construction.
    pub async fn open(options: InstallOptions) -> Self {
        let state = json!({"driver":"codex","operationId":null,"phase":"idle","downloadedBytes":0,
            "totalBytes":options.asset.as_ref().map(|asset| asset.archive_bytes),"version":options.asset.as_ref().map(|asset| asset.version.clone()),
            "installedVersion":null,"executablePath":null,"canRemove":false,"message":null});
        let (changes, _) = broadcast::channel(256);
        let installation = Self(Arc::new(Inner {
            managed: options.managed_directory,
            asset: options.asset,
            trusted: options.trusted_asset,
            path: options.path,
            platform: options.platform,
            arch: options.arch,
            gate: tokio::sync::Mutex::new(()),
            state: Mutex::new(state),
            changes,
            running: Mutex::new(None),
            local: Mutex::new(HashMap::new()),
            leases: AtomicUsize::new(0),
        }));
        installation.scan().await;
        installation
    }
    pub async fn open_for_host(managed_directory: PathBuf, path: Option<String>) -> Self {
        let (platform, arch) = host_names();
        Self::open(InstallOptions { managed_directory, asset: release_asset(platform, arch), trusted_asset: false, path, platform, arch }).await
    }
    async fn scan(&self) {
        self.update(|state| state["canRemove"] = json!(self.0.managed.exists()));
        if self.reuse_local().await {
            return;
        }
        if !self.active_path().exists() {
            return;
        }
        let scanned = async {
            let active: ActiveRelease = self.read_record(&self.active_path())?;
            self.completed_release(&active.version)
        };
        match scanned.await {
            Ok(installed) => self.update(|state| {
                state["installedVersion"] = json!(installed.version);
                state["executablePath"] = json!(installed.path);
                state["source"] = json!("managed");
            }),
            Err(_) => self.update(|state| {
                state["phase"] = json!("failed");
                state["message"] = json!("The managed Codex runtime is incomplete. Remove it and reinstall.");
            }),
        }
    }
    fn active_path(&self) -> PathBuf {
        self.0.managed.join("active.json")
    }
    fn executable_name(&self) -> &'static str {
        if self.0.platform == "win32" { "codex.exe" } else { "codex" }
    }
    fn windows(&self) -> bool {
        self.0.platform == "win32"
    }

    // ---- state ----

    fn update(&self, change: impl FnOnce(&mut Value)) {
        let mut state = self.0.state.lock().unwrap();
        change(&mut state);
        let _ = self.0.changes.send(Arc::new(state.clone()));
    }
    fn replace(&self, next: Value) {
        self.update(|state| *state = next);
    }
    pub fn state(&self) -> Value {
        self.0.state.lock().unwrap().clone()
    }
    /// The current state, then every change; both under one lock so none is missed between them.
    pub fn subscribe(&self) -> (Value, broadcast::Receiver<Arc<Value>>) {
        let state = self.0.state.lock().unwrap();
        (state.clone(), self.0.changes.subscribe())
    }

    // ---- what is installed ----

    fn read_record<T: serde::de::DeserializeOwned>(&self, file: &Path) -> R<T> {
        let info = std::fs::metadata(file)?;
        if !info.is_file() || info.len() > 8192 {
            return Err(InstallError::new("resolve", "The managed Codex installation record is invalid. Reinstall Codex."));
        }
        Ok(serde_json::from_slice(&std::fs::read(file)?)?)
    }
    fn from_directory(&self, directory: &Path, version: &str, target: &str) -> R<Executable> {
        let manifest: PackageManifest = self.read_record(&directory.join("codex-package.json"))?;
        let name = self.executable_name();
        if manifest.layout_version != 1 || !valid_version(&manifest.version) || manifest.version != version || manifest.target != target || manifest.entrypoint != format!("bin/{name}") {
            return Err(InstallError::new("verify", "The downloaded Codex package does not match the expected release."));
        }
        let exe = if self.windows() { ".exe" } else { "" };
        for relative in [format!("bin/{name}"), format!("bin/codex-code-mode-host{exe}"), format!("codex-path/rg{exe}")] {
            let info = std::fs::metadata(directory.join(&relative))?;
            #[cfg(unix)]
            let executable = {
                use std::os::unix::fs::PermissionsExt;
                info.permissions().mode() & 0o111 != 0
            };
            #[cfg(not(unix))]
            let executable = true;
            if !info.is_file() || info.len() == 0 || !executable {
                return Err(InstallError::new("verify", "The managed Codex package is incomplete. Reinstall Codex."));
            }
        }
        Ok(Executable { path: directory.join("bin").join(name), source: "managed", version: version.into() })
    }
    fn completed_release(&self, version: &str) -> R<Executable> {
        if !valid_version(version) {
            return Err(InstallError::raw());
        }
        let directory = self.0.managed.join(version);
        let record: InstalledRelease = self.read_record(&directory.join(".install-complete.json"))?;
        if record.version != version {
            return Err(InstallError::new("resolve", "The managed Codex installation record has the wrong version."));
        }
        self.from_directory(&directory, version, &record.target)
    }
    fn supported(version: &str) -> Option<Value> {
        maintenance::compatibility_advisory("codex", Some(version), env!("CARGO_PKG_VERSION"))
    }
    fn resolve_managed(&self) -> R<Executable> {
        let resolve = || -> R<Executable> {
            let active: ActiveRelease = self.read_record(&self.active_path())?;
            let executable = self.completed_release(&active.version)?;
            let advisory = Self::supported(&executable.version);
            if advisory.as_ref().and_then(|advisory| advisory["status"].as_str()) != Some("supported") {
                let message = advisory.as_ref().and_then(|advisory| advisory["message"].as_str()).unwrap_or("Update the managed Codex installation to continue.");
                return Err(InstallError::new("resolve", message));
            }
            Ok(executable)
        };
        resolve().map_err(|error| error.or("resolve", "Codex is not installed in T3 Code. Install it to continue."))
    }
    /// A supported `codex` on PATH, outside T3's own download. Launcher symlinks stay intact: version-manager
    /// shims dispatch by their invoked name.
    async fn resolve_local(&self) -> Option<Executable> {
        let mut env: HashMap<String, String> = std::env::vars().collect();
        if let Some(path) = &self.0.path {
            env.insert("PATH".into(), path.clone());
        }
        let found = maintenance::resolve_command_path("codex", &env, Platform::host())?;
        let real = std::fs::canonicalize(&found).ok()?;
        // A PATH entry pointing into T3's download remains a managed installation.
        let managed = std::fs::canonicalize(&self.0.managed).unwrap_or_else(|_| self.0.managed.clone());
        if real.starts_with(&managed) {
            return None;
        }
        let info = std::fs::metadata(&found).ok()?;
        #[cfg(unix)]
        let inode = {
            use std::os::unix::fs::MetadataExt;
            info.ino()
        };
        #[cfg(not(unix))]
        let inode = 0u64;
        let modified = info.modified().ok().and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok()).map(|age| age.as_millis()).unwrap_or(0);
        let fingerprint = format!("{}:{}:{modified}:{inode}", real.display(), info.len());
        let cached = self.0.local.lock().unwrap().get(&found).filter(|(known, _)| *known == fingerprint).map(|(_, executable)| executable.clone());
        let executable = match cached {
            Some(executable) => executable,
            None => {
                let probed = tokio::time::timeout(Duration::from_secs(5), async {
                    let program = Path::new(&found);
                    let output = run_capture(program, &["--version"]).await?;
                    static BANNER: OnceLock<Regex> = OnceLock::new();
                    let banner = BANNER.get_or_init(|| Regex::new(r"^codex-cli ([0-9]+\.[0-9]+\.[0-9]+)$").unwrap());
                    let version = banner.captures(output.trim()).map(|captures| captures[1].to_owned()).ok_or_else(InstallError::raw)?;
                    run_capture(program, &["app-server", "--help"]).await?;
                    Ok::<_, InstallError>(Executable { path: PathBuf::from(&found), source: "local", version })
                })
                .await
                .ok()
                .and_then(Result::ok);
                self.0.local.lock().unwrap().insert(found.clone(), (fingerprint, probed.clone()));
                probed
            }
        };
        // Probes are cached, but classified against the current policy on every resolve.
        let executable = executable?;
        (Self::supported(&executable.version)?["status"] == "supported").then_some(executable)
    }
    /// A local install satisfies the setup: nothing is downloaded.
    async fn reuse_local(&self) -> bool {
        let Some(local) = self.resolve_local().await else {
            return false;
        };
        self.update(|state| {
            state["phase"] = json!("succeeded");
            state["source"] = json!("local");
            state["operationId"] = Value::Null;
            state["installedVersion"] = json!(local.version);
            state["executablePath"] = json!(local.path);
            state["downloadedBytes"] = json!(0);
            state["totalBytes"] = Value::Null;
            state["message"] = Value::Null;
        });
        true
    }
    /// A supported local `codex` first, else the active managed release.
    pub async fn resolve(&self) -> Result<Executable, InstallError> {
        match self.resolve_local().await {
            Some(local) => Ok(local),
            None => self.resolve_managed(),
        }
    }

    /// `resolve` for something that goes on to run the executable: the lease keeps the managed runtime from being removed under it.
    /// Taken under the gate `remove` holds, so a removal cannot slip in between the resolve and the lease.
    pub async fn acquire(&self) -> Result<(Executable, Lease), InstallError> {
        let _gate = self.0.gate.lock().await;
        let executable = self.resolve().await?;
        self.0.leases.fetch_add(1, Ordering::SeqCst);
        Ok((executable, Lease(Arc::new(LeaseGuard(self.0.clone())))))
    }

    // ---- install ----

    async fn validate(&self, executable: &Executable, version: &str) -> R<()> {
        let check = async {
            let output = run_capture(&executable.path, &["--version"]).await?;
            if output.trim() != format!("codex-cli {version}") {
                return Err(InstallError::new("verify", "The downloaded Codex executable has the wrong version."));
            }
            Ok(())
        };
        tokio::time::timeout(Duration::from_secs(90), check)
            .await
            .map_err(|_| InstallError::raw())?
            .map_err(|error| error.or("verify", "The downloaded Codex runtime could not start."))
    }
    /// Point `active.json` at an installed version. Synchronous on purpose: a cancellation cannot land halfway.
    fn activate(&self, version: &str) -> R<()> {
        let executable = self.completed_release(version)?;
        let temporary = Staging::create(&self.0.managed, ".active-")?;
        let pointer = temporary.0.join("active.json");
        write_new(&pointer, json!({"version":version}).to_string().as_bytes())?;
        std::fs::rename(&pointer, self.active_path())?;
        self.update(|state| {
            state["phase"] = json!("succeeded");
            state["source"] = json!("managed");
            state["installedVersion"] = json!(version);
            state["executablePath"] = json!(executable.path);
            state["message"] = Value::Null;
        });
        Ok(())
    }
    async fn download(&self, release: &ReleaseAsset, archive: &Path) -> R<()> {
        use tokio::io::AsyncWriteExt;
        let client = reqwest::Client::builder()
            // Never follow a redirect from https down to plain http.
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                let secure_origin = attempt.previous().first().is_some_and(|first| first.scheme() == "https");
                if attempt.previous().len() > 5 || (secure_origin && attempt.url().scheme() != "https") { attempt.stop() } else { attempt.follow() }
            }))
            .build()?;
        let mut response = client.get(&release.url).send().await?.error_for_status()?;
        let mut file = open_new(archive).await?;
        let (mut hash, mut downloaded) = (Sha256::new(), 0u64);
        let mut last_progress = tokio::time::Instant::now();
        while let Some(chunk) = response.chunk().await? {
            downloaded += chunk.len() as u64;
            if downloaded > release.archive_bytes {
                return Err(InstallError::new("download", "The Codex download exceeded the expected release size."));
            }
            hash.update(&chunk);
            file.write_all(&chunk).await?;
            if last_progress.elapsed() >= Duration::from_millis(250) || downloaded == release.archive_bytes {
                last_progress = tokio::time::Instant::now();
                self.update(|state| state["downloadedBytes"] = json!(downloaded));
            }
        }
        file.flush().await?;
        if downloaded != release.archive_bytes || format!("{:x}", hash.finalize()) != release.sha256 {
            return Err(InstallError::new("download", "The Codex download failed its size or SHA-256 check. Nothing was installed."));
        }
        Ok(())
    }
    async fn install(&self, release: &ReleaseAsset) -> R<()> {
        if !valid_version(&release.version) {
            return Err(InstallError::raw());
        }
        std::fs::create_dir_all(&self.0.managed)?;
        self.update(|state| state["canRemove"] = json!(true));
        let destination = self.0.managed.join(&release.version);
        if destination.exists() {
            let existing = self.completed_release(&release.version)?;
            let record: InstalledRelease = self.read_record(&destination.join(".install-complete.json"))?;
            if record.sha256 != release.sha256 || record.target != release.target {
                return Err(InstallError::new("verify", "The existing managed Codex release differs from the official package. Remove it and reinstall."));
            }
            self.validate(&existing, &release.version).await?;
            return self.activate(&release.version);
        }
        let _staging = Staging::create(&self.0.managed, ".install-")?;
        let staging_dir = _staging.0.clone();
        let archive = staging_dir.join("download.tar.gz");
        let runtime = staging_dir.join("runtime");
        std::fs::create_dir(&runtime)?;
        tokio::time::timeout(Duration::from_secs(45 * 60), self.download(release, &archive)).await.map_err(|_| InstallError::raw())??;
        self.update(|state| {
            state["phase"] = json!("extracting");
            state["message"] = json!("Extracting Codex.");
        });
        let tar = Path::new("tar");
        let archive_text = archive.to_string_lossy().into_owned();
        let listing = run_capture(tar, &["-tzf", &archive_text]).await?;
        let verbose = run_capture(tar, &["-tvzf", &archive_text]).await?;
        let entries: Vec<&str> = listing.trim().split('\n').collect();
        let types: Vec<&str> = verbose.trim().split('\n').collect();
        if unsafe_archive(&entries, &types) {
            return Err(InstallError::new("extract", "The Codex archive contains unsafe entries."));
        }
        run_capture(tar, &["-xzf", &archive_text, "-C", &runtime.to_string_lossy()]).await?;
        self.update(|state| {
            state["phase"] = json!("verifying");
            state["message"] = json!("Checking Codex.");
        });
        let executable = self.from_directory(&runtime, &release.version, &release.target)?;
        self.validate(&executable, &release.version).await?;
        write_new(&runtime.join(".install-complete.json"), json!({"version":release.version,"target":release.target,"sha256":release.sha256}).to_string().as_bytes())?;
        std::fs::rename(&runtime, &destination)?;
        // The archive is no longer needed; it is gone before clients are told the install is done.
        drop(_staging);
        self.activate(&release.version)
    }
    pub async fn start(&self) -> Result<Value, InstallError> {
        let _gate = self.0.gate.lock().await;
        let failure = |error: InstallError| error.or("start", "Could not start the Codex installation.");
        let current = self.state();
        if is_running(&current) {
            return Ok(current);
        }
        if self.reuse_local().await {
            return Ok(self.state());
        }
        let Some(asset) = self.0.asset.clone() else {
            return Err(InstallError::new("start", &format!("OpenAI does not publish a Codex runtime for {}-{}. Use a supported remote environment or a custom executable.", self.0.platform, self.0.arch)));
        };
        // A download goes to the vendor's release host only; other sources need code that supplies them.
        if !self.0.trusted && !asset.url.starts_with(OFFICIAL_RELEASE_PREFIX) {
            return Err(failure(InstallError::raw()));
        }
        let operation_id = uuid::Uuid::new_v4().to_string();
        let next = json!({"driver":"codex","operationId":operation_id,"phase":"downloading","downloadedBytes":0,"totalBytes":asset.archive_bytes,
            "version":asset.version,"installedVersion":current["installedVersion"],"canRemove":current["canRemove"],"message":"Downloading Codex."});
        self.replace(next.clone());
        let installation = self.clone();
        let operation = operation_id.clone();
        let task = tokio::spawn(async move {
            let result = Box::pin(installation.install(&asset)).await;
            if let Err(error) = result {
                let error = error.or("install", "Could not install Codex. Check disk space and directory access, then try again.");
                installation.update(|state| {
                    if state["operationId"] == operation.as_str() && state["phase"] != "succeeded" {
                        state["phase"] = json!("failed");
                        state["message"] = json!(error.detail);
                    }
                });
            }
            let mut running = installation.0.running.lock().unwrap();
            if running.as_ref().is_some_and(|(id, _)| *id == operation) {
                running.take();
            }
        });
        *self.0.running.lock().unwrap() = Some((operation_id, task));
        Ok(next)
    }
    pub async fn cancel(&self, operation_id: &str) -> Result<Value, InstallError> {
        let _gate = self.0.gate.lock().await;
        let current = self.state();
        if current["operationId"] != operation_id {
            return Err(InstallError::new("cancel", "This installation is no longer current. Refresh its status before cancelling."));
        }
        if is_running(&current) {
            let task = {
                let mut running = self.0.running.lock().unwrap();
                if running.as_ref().is_some_and(|(id, _)| id == operation_id) { running.take().map(|(_, task)| task) } else { None }
            };
            if let Some(task) = task {
                task.abort();
                let _ = task.await;
                self.update(|state| {
                    if state["operationId"] == operation_id && state["phase"] != "succeeded" {
                        state["phase"] = json!("cancelled");
                        state["message"] = json!("Installation cancelled. The previous runtime is unchanged.");
                    }
                });
            }
        }
        Ok(self.state())
    }
    /// Remove the managed runtime, and nothing else. `protected` are binary paths some instance is configured to
    /// run: if one lies inside the managed runtime, nothing is removed.
    pub async fn remove(&self, protected: &[String]) -> Result<(), InstallError> {
        let _gate = self.0.gate.lock().await;
        let default = "Could not remove the managed Codex runtime. Check for open processes and try again.";
        if is_running(&self.state()) || self.0.leases.load(Ordering::SeqCst) > 0 {
            return Err(InstallError::new("remove", "Stop Codex sessions and sign-in flows before removing its managed runtime."));
        }
        if let Ok(managed) = std::fs::canonicalize(&self.0.managed) {
            for binary in protected {
                let candidate = std::fs::canonicalize(binary).unwrap_or_else(|_| std::path::absolute(binary).unwrap_or_else(|_| PathBuf::from(binary)));
                if candidate.starts_with(&managed) {
                    return Err(InstallError::new("remove", "A provider instance uses a custom path inside managed Codex. Clear that path before removing it."));
                }
            }
        }
        match std::fs::symlink_metadata(&self.0.managed) {
            Ok(info) if info.file_type().is_symlink() => std::fs::remove_file(&self.0.managed),
            Ok(_) => std::fs::remove_dir_all(&self.0.managed),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
        .map_err(|_| InstallError::new("remove", default))?;
        self.update(|state| {
            state["operationId"] = Value::Null;
            state["phase"] = json!("idle");
            state["downloadedBytes"] = json!(0);
            state["installedVersion"] = Value::Null;
            state["executablePath"] = Value::Null;
            state["source"] = Value::Null;
            state["canRemove"] = json!(false);
            state["message"] = Value::Null;
        });
        self.reuse_local().await;
        Ok(())
    }
}
fn write_new(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(contents)
}
async fn open_new(path: &Path) -> std::io::Result<tokio::fs::File> {
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    options.open(path).await
}

// ---- the service the socket calls (providerInstallation.ts) ----

#[derive(Default)]
pub struct Slot(tokio::sync::OnceCell<CodexInstallation>);

impl Management {
    /// The Codex installation service, opened on first use from the tools directory.
    pub async fn codex_installation(&self) -> Result<CodexInstallation, InstallError> {
        let options = self.options();
        let Some(tools) = options.tools_dir.clone() else {
            return Err(InstallError::new("install", "Managed installation is not available in this build."));
        };
        let installation = self
            .install_slot()
            .0
            .get_or_init(|| async move {
                let managed = tools.join("codex");
                match options.codex_release {
                    Some(asset) => {
                        let (platform, arch) = host_names();
                        CodexInstallation::open(InstallOptions { managed_directory: managed, asset, trusted_asset: true, path: options.codex_path, platform, arch }).await
                    }
                    None => CodexInstallation::open_for_host(managed, options.codex_path).await,
                }
            })
            .await;
        Ok(installation.clone())
    }
}
impl Management {
    /// The Antigravity installation service, opened on first use from the tools directory. Without one nothing
    /// can be installed, but a custom executable or one on the `PATH` still resolves.
    pub async fn antigravity_installation(&self) -> Result<crate::antigravity_install::AntigravityInstallation, crate::antigravity_install::InstallError> {
        use crate::antigravity_install as antigravity;
        let options = self.options();
        let installation = self
            .antigravity_slot()
            .get_or_init(|| async move {
                let tools = options.tools_dir.clone().unwrap_or_else(|| std::env::temp_dir().join("t3-unmanaged-tools"));
                match options.antigravity_release {
                    Some(asset) => {
                        let (platform, arch) = antigravity::host_names();
                        antigravity::AntigravityInstallation::open(antigravity::InstallOptions { managed_directory: tools.join("antigravity-acp").join(format!("{platform}-{arch}")), asset, trusted_asset: true, platform, arch }).await
                    }
                    None => antigravity::AntigravityInstallation::open_for_host(&tools).await,
                }
            })
            .await;
        Ok(installation.clone())
    }
}
fn setup_error(instance: &ProviderInstanceId, operation: &str, detail: &str) -> ProviderSetupError {
    ProviderSetupError { tag: ProviderSetupErrorTag::ProviderSetupError, instance_id: instance.clone(), operation: operation.into(), detail: detail.into(), cause: None }
}
/// The installation behind a provider instance, if managed installation applies to it. Only Codex
/// instances with the managed setup and no custom executable qualify; any other driver — including those
/// this build has no adapter for — answers as the source does for an instance it cannot install.
async fn require_instance(registry: &ProviderRegistry, instance: &ProviderInstanceId, operation: &str, managed_only: bool) -> Result<CodexInstallation, ProviderSetupError> {
    let error = |detail: &str| setup_error(instance, operation, detail);
    let Some(codex) = registry.codex_instance(instance.as_str()) else {
        return Err(error("Managed installation is not available for this provider instance."));
    };
    if codex.config.setup_mode != Some(t3_contracts::CodexSetupMode::Managed) {
        return Err(error("Choose managed setup to install Codex in T3 Code."));
    }
    let binary = codex.config.binary_path.as_str();
    if managed_only && !binary.is_empty() && binary != "codex" {
        return Err(error("This instance uses a custom executable. Clear its binary path to manage installation in T3 Code."));
    }
    registry.management().codex_installation().await.map_err(|failure| setup_error(instance, &failure.operation, &failure.detail))
}
fn failure(instance: &ProviderInstanceId) -> impl Fn(InstallError) -> ProviderSetupError + '_ {
    move |error| setup_error(instance, &error.operation, &error.detail)
}
/// The Antigravity installation behind an instance, when it is one (`ProviderInstallation.requireInstance`). A custom
/// executable is not T3's to install or remove.
async fn antigravity_installation(registry: &ProviderRegistry, instance: &ProviderInstanceId, operation: &str, managed_only: bool) -> Result<Option<crate::antigravity_install::AntigravityInstallation>, ProviderSetupError> {
    let Some((_, antigravity)) = registry.antigravity_auth_entry(instance.as_str()) else { return Ok(None) };
    if managed_only && !antigravity.config.binary_path.as_str().is_empty() {
        return Err(setup_error(instance, operation, "This instance uses a custom executable. Clear its binary path to manage installation in T3 Code."));
    }
    antigravity.installation().await.map(Some).map_err(|failure| setup_error(instance, &failure.operation, &failure.detail))
}
fn antigravity_failure(instance: &ProviderInstanceId) -> impl Fn(crate::antigravity_install::InstallError) -> ProviderSetupError + '_ {
    move |error| setup_error(instance, &error.operation, &error.detail)
}
pub async fn start(registry: &ProviderRegistry, instance: &ProviderInstanceId) -> Result<Value, ProviderSetupError> {
    if let Some(installation) = antigravity_installation(registry, instance, "install", true).await? {
        let state = installation.start().await.map_err(antigravity_failure(instance))?;
        // The provider list reads "installed" once the download lands, without waiting for a manual refresh.
        let (registry, instance) = (registry.clone(), instance.clone());
        let (current, mut changes) = installation.subscribe();
        let operation = state["operationId"].clone();
        tokio::spawn(async move {
            let mut seen = current;
            loop {
                if seen["phase"] == "succeeded" && (operation.is_null() || seen["operationId"] == operation) {
                    let _ = registry.refresh_instance(instance.as_str()).await;
                    return;
                }
                if matches!(seen["phase"].as_str(), Some("failed" | "cancelled")) || (!operation.is_null() && seen["operationId"] != operation && seen["phase"] == "idle") {
                    return;
                }
                match changes.recv().await {
                    Ok(next) => seen = next.as_ref().clone(),
                    Err(broadcast::error::RecvError::Lagged(_)) => seen = installation.state(),
                    Err(_) => return,
                }
            }
        });
        return Ok(state);
    }
    let installation = require_instance(registry, instance, "install", true).await?;
    let state = installation.start().await.map_err(failure(instance))?;
    // The provider list reads "installed" once the download lands, without waiting for a manual refresh.
    let (registry, instance) = (registry.clone(), instance.clone());
    let (current, mut changes) = installation.subscribe();
    let operation = state["operationId"].clone();
    tokio::spawn(async move {
        let mut seen = current;
        loop {
            if seen["phase"] == "succeeded" && (operation.is_null() || seen["operationId"] == operation) {
                let _ = registry.refresh_instance(instance.as_str()).await;
                return;
            }
            if matches!(seen["phase"].as_str(), Some("failed" | "cancelled")) || (!operation.is_null() && seen["operationId"] != operation && seen["phase"] == "idle") {
                return;
            }
            match changes.recv().await {
                Ok(next) => seen = next.as_ref().clone(),
                Err(broadcast::error::RecvError::Lagged(_)) => seen = installation.state(),
                Err(_) => return,
            }
        }
    });
    Ok(state)
}
pub async fn cancel(registry: &ProviderRegistry, input: &ProviderInstallCancelInput) -> Result<Value, ProviderSetupError> {
    if let Some(installation) = antigravity_installation(registry, &input.instance_id, "cancel-install", false).await? {
        return installation.cancel(input.operation_id.0.as_str()).await.map_err(antigravity_failure(&input.instance_id));
    }
    let installation = require_instance(registry, &input.instance_id, "cancel-install", false).await?;
    installation.cancel(input.operation_id.0.as_str()).await.map_err(failure(&input.instance_id))
}
/// The current state and its changes. The first error is the stream's failure.
pub async fn subscribe(registry: &ProviderRegistry, instance: &ProviderInstanceId) -> Result<(Value, broadcast::Receiver<Arc<Value>>), ProviderSetupError> {
    if let Some(installation) = antigravity_installation(registry, instance, "observe-install", false).await? {
        return Ok(installation.subscribe());
    }
    Ok(require_instance(registry, instance, "observe-install", false).await?.subscribe())
}
pub async fn remove(registry: &ProviderRegistry, instance: &ProviderInstanceId) -> Result<Value, ProviderSetupError> {
    if let Some(installation) = antigravity_installation(registry, instance, "remove-install", true).await? {
        // Binaries other instances are configured to run, resolved as they would be launched.
        let protected: Vec<String> = registry.settings_binaries().into_iter().flat_map(|(binary, env)| {
            let resolved = maintenance::resolve_command_path(&binary, &env, Platform::host());
            std::iter::once(binary).chain(resolved)
        }).collect();
        installation.remove(&protected).await.map_err(antigravity_failure(instance))?;
        // Every Antigravity instance now reads the runtime as gone.
        for id in registry.antigravity_instance_ids() {
            let _ = registry.refresh_instance(&id).await;
        }
        return Ok(installation.state());
    }
    let installation = require_instance(registry, instance, "remove-install", true).await?;
    // Binaries other instances are configured to run, resolved as they would be launched.
    let protected: Vec<String> = registry
        .settings_binaries()
        .into_iter()
        .flat_map(|(binary, env)| {
            let resolved = maintenance::resolve_command_path(&binary, &env, Platform::host());
            std::iter::once(binary).chain(resolved)
        })
        .collect();
    installation.remove(&protected).await.map_err(failure(instance))?;
    // Every Codex instance now reads the runtime as gone (or as the local install that replaces it).
    for id in registry.codex_instance_ids() {
        let _ = registry.refresh_instance(&id).await;
    }
    Ok(installation.state())
}

fn validated(snapshot: Value) -> Result<Value, crate::provider_process::ProcessError> {
    let typed: t3_contracts::ServerProvider = serde_json::from_value(snapshot).map_err(|error| crate::provider_process::ProcessError::Protocol(error.to_string()))?;
    serde_json::to_value(typed).map_err(|error| crate::provider_process::ProcessError::Protocol(error.to_string()))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    fn fixtures() -> Vec<Value> {
        include_str!("../tests/fixtures/provider-install.jsonl").lines().map(|line| serde_json::from_str(line).unwrap()).collect()
    }
    #[test]
    fn release_table_archive_rule_and_version_pattern_match_the_original_source() {
        let mut checked = 0;
        for fixture in fixtures() {
            match fixture["kind"].as_str().unwrap() {
                "release" => assert_eq!(release_asset(fixture["platform"].as_str().unwrap(), fixture["arch"].as_str().unwrap()).map(|asset| asset.to_json()).unwrap_or(Value::Null), fixture["expected"], "{fixture}"),
                "entries" => {
                    let entries: Vec<&str> = fixture["entries"].as_array().unwrap().iter().map(|entry| entry.as_str().unwrap()).collect();
                    let types: Vec<&str> = fixture["types"].as_array().unwrap().iter().map(|entry| entry.as_str().unwrap()).collect();
                    assert_eq!(unsafe_archive(&entries, &types), fixture["expected"].as_bool().unwrap(), "{fixture}");
                }
                "version" => assert_eq!(valid_version(fixture["text"].as_str().unwrap()), fixture["expected"].as_bool().unwrap(), "{fixture}"),
                other => panic!("{other}"),
            }
            checked += 1;
        }
        assert_eq!(checked, 61);
    }
    #[tokio::test]
    async fn a_download_from_anywhere_but_the_official_release_host_is_never_started() {
        let directory = tempfile::tempdir().unwrap();
        let asset = ReleaseAsset { version: "0.156.1".into(), target: "t".into(), url: "http://127.0.0.1:9/codex.tar.gz".into(), sha256: "0".repeat(64), archive_bytes: 1 };
        // A listener that fails the test if anything connects to it.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let asset = ReleaseAsset { url: format!("http://{}/codex.tar.gz", listener.local_addr().unwrap()), ..asset };
        let installation = CodexInstallation::open(InstallOptions { managed_directory: directory.path().join("codex"), asset: Some(asset), trusted_asset: false, path: Some(directory.path().join("empty").to_string_lossy().into_owned()), platform: "linux", arch: "x64" }).await;
        let refused = installation.start().await.unwrap_err();
        assert_eq!((refused.operation.as_str(), refused.detail.as_str()), ("start", "Could not start the Codex installation."));
        assert_eq!(installation.state()["phase"], "idle");
        assert!(listener.accept().is_err(), "nothing may connect");
        assert!(!directory.path().join("codex").exists());
    }
    #[test]
    fn every_official_asset_comes_from_the_one_release_host() {
        for (platform, arch) in [("darwin", "arm64"), ("darwin", "x64"), ("linux", "arm64"), ("linux", "x64"), ("win32", "arm64"), ("win32", "x64")] {
            let asset = release_asset(platform, arch).unwrap();
            assert!(asset.url.starts_with(OFFICIAL_RELEASE_PREFIX) && asset.url.ends_with(".tar.gz"), "{asset:?}");
            assert_eq!(asset.sha256.len(), 64);
        }
    }
}
