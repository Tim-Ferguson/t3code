//! Managed Antigravity installation (`AntigravityInstallation.ts`, `antigravityRelease.ts`): where the runtime
//! lives, how a download is verified, the progress events clients watch, cancellation and removal.
//!
//! A managed install lives in `<tools>/antigravity-acp/<platform>-<arch>`: `versions/<sha256>/` holds the agent
//! and its harness with `.install-complete.json`, and `active.json` names the release in use. Google publishes one
//! pinned archive per platform. It is downloaded from the one official host named below, checked against its
//! pinned size and SHA-256, and only then unpacked: exactly the agent and its harness, each checked against its
//! pinned size. Nothing is executed before the archive has passed, and the agent is asked to identify itself
//! before the release is activated.
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::broadcast;

/// The only place a managed Antigravity runtime is downloaded from.
pub const OFFICIAL_RELEASE_PREFIX: &str = "https://dl.google.com/agy-extensions/releases/";
const RELEASE_RECORD: &str = ".install-complete.json";
const FREE_SPACE_MARGIN: u64 = 256 * 1024 * 1024;
const RECORD_MAX_BYTES: u64 = 8 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct File {
    pub name: String,
    pub bytes: u64,
}
#[derive(Clone, Debug, PartialEq)]
pub struct ReleaseAsset {
    pub version: String,
    pub url: String,
    pub sha256: String,
    pub archive_bytes: u64,
    pub executable: File,
    pub harness: File,
}
/// The pinned releases (`antigravityRelease.ts`). `platform` and `arch` are Node's names.
pub fn release_asset(platform: &str, arch: &str) -> Option<ReleaseAsset> {
    let (url, sha256, archive, executable, harness) = match (platform, arch) {
        ("darwin", "arm64") => ("macos/agy-acp-server-1.3.0-darwin-arm64.zip", "7cd97045f7b4fe81175a107cdf16f9c51484e3c78a5162cae415338bb6aa5b88", 111_456_962, ("agy_acp_server.par", 278_535_456), ("localharness_external", 118_611_392)),
        ("darwin", "x64") => ("macos/agy-acp-server-1.3.0-darwin-x86_64.zip", "bb23956b89984bf5d354af2c3725e6c57f0cc1b7228e77a0e91c9c2bc1d47646", 117_245_544, ("agy_acp_server.par", 282_840_688), ("localharness_external", 124_175_392)),
        ("linux", "x64") => ("linux/agy-acp-server-1.3.0-linux-x86_64.zip", "9fb60956af0a9d76220a4db91ca9ac88e2a2372ad68f985ab5fceace6b825b96", 333_727_150, ("agy_acp_server.par", 926_533_965), ("localharness_external", 130_388_040)),
        ("linux", "arm64") => ("linux/agy-acp-server-1.3.0-linux-arm64.zip", "500b0bc0fb858e88f4df404d4cedf80bf9298c178291e39e383d6c50b111cbdf", 321_690_363, ("agy_acp_server.par", 930_848_992), ("localharness_external", 123_224_968)),
        ("win32", "x64") => ("windows/agy-acp-server-1.3.0-windows-x86_64.zip", "65215e0688681fa3116e048a9eab27ef53af1bbd6f3da3f1c52bd4911d8b17f9", 124_509_787, ("agy_acp_server.exe", 81_437_336), ("localharness_external.exe", 145_548_952)),
        ("win32", "arm64") => ("windows/agy-acp-server-1.3.0-windows-arm64.zip", "4a0f469720e9beb9438a979f543fdbfad5022ebe0992c052c590bd78b3144ca3", 124_654_803, ("agy_acp_server.exe", 85_893_472), ("localharness_external.exe", 135_640_216)),
        _ => return None,
    };
    let file = |(name, bytes): (&str, u64)| File { name: name.into(), bytes };
    Some(ReleaseAsset { version: "1.3.0".into(), url: format!("{OFFICIAL_RELEASE_PREFIX}{url}"), sha256: sha256.into(), archive_bytes: archive, executable: file(executable), harness: file(harness) })
}
/// Node's names for this host.
pub fn host_names() -> (&'static str, &'static str) {
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
fn names(platform: &str) -> (&'static str, &'static str) {
    if platform == "win32" { ("agy_acp_server.exe", "localharness_external.exe") } else { ("agy_acp_server.par", "localharness_external") }
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

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActiveRelease {
    release_id: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InstalledRelease {
    release_id: String,
    version: String,
    executable: RecordFile,
    harness: RecordFile,
}
#[derive(Deserialize)]
struct RecordFile {
    name: String,
    bytes: u64,
}
fn release_id(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// The agent and the harness it needs beside it.
#[derive(Clone, Debug)]
pub struct Executable {
    pub executable_path: PathBuf,
    pub harness_path: PathBuf,
    pub source: &'static str,
    pub version: Option<String>,
    pub managed_version_directory: Option<PathBuf>,
}

pub struct InstallOptions {
    /// `<tools>/antigravity-acp/<platform>-<arch>`.
    pub managed_directory: PathBuf,
    /// `None` for a platform Google publishes nothing for. `trusted` is set only by code that supplies its own
    /// asset (tests), which waives the official-host rule.
    pub asset: Option<ReleaseAsset>,
    pub trusted_asset: bool,
    pub platform: &'static str,
    pub arch: &'static str,
}
struct Inner {
    managed: PathBuf,
    asset: Option<ReleaseAsset>,
    trusted: bool,
    platform: &'static str,
    arch: &'static str,
    gate: tokio::sync::Mutex<()>,
    state: Mutex<Value>,
    changes: broadcast::Sender<Arc<Value>>,
    running: Mutex<Option<(String, tokio::task::JoinHandle<()>)>>,
    leases: Mutex<HashMap<PathBuf, usize>>,
}
#[derive(Clone)]
pub struct AntigravityInstallation(Arc<Inner>);
/// Holds a managed release in use until the process that runs it has ended.
pub struct Lease {
    installation: AntigravityInstallation,
    directory: Option<PathBuf>,
    pub executable: Executable,
}
impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(directory) = &self.directory {
            let mut leases = self.installation.0.leases.lock().unwrap();
            let remaining = leases.get(directory).copied().unwrap_or(1) - 1;
            if remaining > 0 {
                leases.insert(directory.clone(), remaining);
            } else {
                leases.remove(directory);
            }
        }
    }
}
fn is_running(state: &Value) -> bool {
    matches!(state["phase"].as_str(), Some("downloading" | "extracting" | "verifying"))
}
/// Removes a staging directory when the installation that owns it ends, however it ends (including cancellation).
struct Staging(PathBuf);
impl Staging {
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
fn is_executable_file(path: &Path, bytes: Option<u64>) -> bool {
    let Ok(info) = std::fs::metadata(path) else { return false };
    #[cfg(unix)]
    let executable = {
        use std::os::unix::fs::PermissionsExt;
        info.permissions().mode() & 0o111 != 0
    };
    #[cfg(not(unix))]
    let executable = true;
    info.is_file() && bytes.is_none_or(|bytes| info.len() == bytes) && executable
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

// ---- the archive ----

/// What `AntigravityInstallation.ts` asks of yauzl: the central directory of a local, already verified archive.
struct ZipEntry {
    name: String,
    flags: u16,
    method: u16,
    crc: u32,
    compressed: u64,
    size: u64,
    external_attributes: u32,
    header_offset: u64,
}
fn le16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}
fn le32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}
fn read_central_directory(file: &mut std::fs::File) -> R<Vec<ZipEntry>> {
    let corrupt = || InstallError::new("extract", "The archive could not be read.");
    let length = file.metadata()?.len();
    let tail_length = length.min(22 + 65_535);
    file.seek(SeekFrom::Start(length - tail_length))?;
    let mut tail = vec![0u8; tail_length as usize];
    file.read_exact(&mut tail)?;
    let end = (0..=tail.len().saturating_sub(22)).rev().find(|&at| tail[at..].starts_with(&[0x50, 0x4b, 0x05, 0x06])).ok_or_else(corrupt)?;
    let (count, size, offset) = (le16(&tail, end + 10) as usize, le32(&tail, end + 12) as u64, le32(&tail, end + 16) as u64);
    // Zip64 archives are not what Google ships; a record that points past 32 bits is refused.
    if le16(&tail, end + 8) as usize != count || count == 0xffff || size == 0xffff_ffff || offset == 0xffff_ffff || offset + size > length || size > 1 << 20 {
        return Err(corrupt());
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut directory = vec![0u8; size as usize];
    file.read_exact(&mut directory)?;
    let mut entries = vec![];
    let mut at = 0;
    for _ in 0..count {
        if directory.len() < at + 46 || directory[at..at + 4] != [0x50, 0x4b, 0x01, 0x02] {
            return Err(corrupt());
        }
        let (name_length, extra_length, comment_length) = (le16(&directory, at + 28) as usize, le16(&directory, at + 30) as usize, le16(&directory, at + 32) as usize);
        let name = directory.get(at + 46..at + 46 + name_length).ok_or_else(corrupt)?;
        let (compressed, size, header_offset) = (le32(&directory, at + 20), le32(&directory, at + 24), le32(&directory, at + 42));
        if compressed == 0xffff_ffff || size == 0xffff_ffff || header_offset == 0xffff_ffff {
            return Err(corrupt());
        }
        entries.push(ZipEntry {
            name: String::from_utf8(name.to_vec()).map_err(|_| corrupt())?,
            flags: le16(&directory, at + 8),
            method: le16(&directory, at + 10),
            crc: le32(&directory, at + 16),
            compressed: compressed as u64,
            size: size as u64,
            external_attributes: le32(&directory, at + 38),
            header_offset: header_offset as u64,
        });
        at += 46 + name_length + extra_length + comment_length;
    }
    Ok(entries)
}
/// Writes one entry to `destination`, never more than `expected` bytes, and checks it is exactly that long
/// and matches the checksum the archive records.
fn extract_entry(file: &mut std::fs::File, entry: &ZipEntry, expected: u64, destination: &Path) -> R<()> {
    use std::io::Write;
    let corrupt = || InstallError::new("extract", "Could not extract the Antigravity runtime.");
    file.seek(SeekFrom::Start(entry.header_offset))?;
    let mut local = [0u8; 30];
    file.read_exact(&mut local)?;
    if local[..4] != [0x50, 0x4b, 0x03, 0x04] {
        return Err(corrupt());
    }
    let skip = le16(&local, 26) as u64 + le16(&local, 28) as u64;
    file.seek(SeekFrom::Current(skip as i64))?;
    let source = file.take(entry.compressed);
    let mut reader: Box<dyn Read> = if entry.method == 0 { Box::new(source) } else { Box::new(flate2::read::DeflateDecoder::new(source)) };
    let mut output = {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o700);
        }
        options.open(destination)?
    };
    let (mut buffer, mut written, mut crc) = (vec![0u8; 1 << 20], 0u64, flate2::Crc::new());
    loop {
        let read = reader.read(&mut buffer).map_err(|_| corrupt())?;
        if read == 0 {
            break;
        }
        written += read as u64;
        if written > expected {
            return Err(InstallError::new("extract", "An archive member exceeded its pinned size."));
        }
        crc.update(&buffer[..read]);
        output.write_all(&buffer[..read])?;
    }
    if written != expected {
        return Err(InstallError::new("extract", "An archive member was truncated."));
    }
    if crc.sum() != entry.crc {
        return Err(corrupt());
    }
    Ok(())
}
fn extract_pair(archive: &Path, asset: &ReleaseAsset, directory: &Path) -> R<()> {
    let mut file = std::fs::File::open(archive).map_err(|_| InstallError::new("extract", "Could not open the verified archive."))?;
    let entries = read_central_directory(&mut file)?;
    if entries.len() != 2 {
        return Err(InstallError::new("extract", "The archive must contain exactly the Antigravity executable and its harness."));
    }
    let mut seen: Vec<&str> = vec![];
    for entry in &entries {
        let expected = [&asset.executable, &asset.harness].into_iter().find(|expected| expected.name == entry.name);
        let unix_type = (entry.external_attributes >> 16) & 0o170000;
        let unsafe_member = match expected {
            None => true,
            Some(expected) => {
                seen.contains(&entry.name.as_str())
                    || entry.name.contains('/')
                    || entry.name.contains('\\')
                    || (unix_type != 0 && unix_type != 0o100000)
                    || entry.external_attributes & 0x10 != 0
                    || entry.flags & 1 != 0
                    || !matches!(entry.method, 0 | 8)
                    || entry.size != expected.bytes
            }
        };
        if unsafe_member {
            return Err(InstallError::new("extract", "The archive contains an unexpected, unsafe, or incorrectly sized member."));
        }
        seen.push(&entry.name);
        extract_entry(&mut file, entry, expected.unwrap().bytes, &directory.join(&entry.name))?;
    }
    if !seen.contains(&asset.executable.name.as_str()) || !seen.contains(&asset.harness.name.as_str()) {
        return Err(InstallError::new("extract", "The archive is missing the Antigravity executable or its harness."));
    }
    Ok(())
}
#[cfg(unix)]
fn free_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statvfs(path.as_ptr(), &mut stat) } == 0).then(|| stat.f_bavail as u64 * stat.f_frsize as u64)
}
#[cfg(not(unix))]
fn free_bytes(_: &Path) -> Option<u64> {
    None
}

impl AntigravityInstallation {
    /// Builds the service and reads what is already installed, as the source does at construction.
    pub async fn open(options: InstallOptions) -> Self {
        let state = json!({"driver":"antigravity","operationId":null,"phase":"idle","downloadedBytes":0,
            "totalBytes":options.asset.as_ref().map(|asset| asset.archive_bytes),"version":options.asset.as_ref().map(|asset| asset.version.clone()),
            "installedVersion":null,"canRemove":false,"message":null});
        let (changes, _) = broadcast::channel(256);
        let installation = Self(Arc::new(Inner {
            managed: options.managed_directory,
            asset: options.asset,
            trusted: options.trusted_asset,
            platform: options.platform,
            arch: options.arch,
            gate: tokio::sync::Mutex::new(()),
            state: Mutex::new(state),
            changes,
            running: Mutex::new(None),
            leases: Mutex::new(HashMap::new()),
        }));
        installation.scan();
        installation
    }
    pub async fn open_for_host(managed_tools: &Path) -> Self {
        let (platform, arch) = host_names();
        Self::open(InstallOptions { managed_directory: managed_tools.join("antigravity-acp").join(format!("{platform}-{arch}")), asset: release_asset(platform, arch), trusted_asset: false, platform, arch }).await
    }
    fn scan(&self) {
        self.update(|state| state["canRemove"] = json!(self.0.managed.exists()));
        if !self.active_path().exists() {
            return;
        }
        let scanned = self.read_record::<ActiveRelease>(&self.active_path()).and_then(|active| self.completed_release(&active.release_id));
        match scanned {
            Ok(installed) => self.update(|state| state["installedVersion"] = json!(installed.version)),
            Err(_) => self.update(|state| {
                state["phase"] = json!("failed");
                state["message"] = json!("The managed Antigravity runtime is incomplete. Remove it and reinstall.");
            }),
        }
    }
    fn active_path(&self) -> PathBuf {
        self.0.managed.join("active.json")
    }
    fn versions(&self) -> PathBuf {
        self.0.managed.join("versions")
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
    pub fn managed_directory(&self) -> &Path {
        &self.0.managed
    }

    // ---- what is installed ----

    fn read_record<T: serde::de::DeserializeOwned>(&self, file: &Path) -> R<T> {
        let info = std::fs::metadata(file)?;
        if !info.is_file() || info.len() > RECORD_MAX_BYTES {
            return Err(InstallError::new("resolve", "The managed runtime record is invalid. Reinstall Antigravity."));
        }
        Ok(serde_json::from_slice(&std::fs::read(file)?)?)
    }
    fn completed_release(&self, id: &str) -> R<Executable> {
        let incomplete = || InstallError::new("resolve", "The managed Antigravity runtime is incomplete. Reinstall it.");
        if !release_id(id) {
            return Err(InstallError::raw());
        }
        let directory = self.versions().join(id);
        let record: InstalledRelease = self.read_record(&directory.join(RELEASE_RECORD))?;
        let (executable_name, harness_name) = names(self.0.platform);
        if record.release_id != id
            || record.executable.name != executable_name
            || record.harness.name != harness_name
            || record.executable.bytes == 0
            || record.harness.bytes == 0
            || record.version.trim().is_empty()
            || !is_executable_file(&directory.join(executable_name), Some(record.executable.bytes))
            || !is_executable_file(&directory.join(harness_name), Some(record.harness.bytes))
        {
            return Err(incomplete());
        }
        Ok(Executable { executable_path: directory.join(executable_name), harness_path: directory.join(harness_name), source: "managed", version: Some(record.version), managed_version_directory: Some(directory) })
    }
    fn from_external(&self, candidate: &Path, source: &'static str) -> R<Option<Executable>> {
        if !is_executable_file(candidate, None) {
            return Ok(None);
        }
        let executable_path = std::fs::canonicalize(candidate)?;
        let directory = executable_path.parent().map(Path::to_path_buf).unwrap_or_default();
        let harness_path = directory.join(names(self.0.platform).1);
        if !is_executable_file(&harness_path, None) {
            return Ok(None);
        }
        if let Ok(versions) = std::fs::canonicalize(self.versions()) {
            if directory.parent() == Some(versions.as_path()) && directory.file_name().and_then(|name| name.to_str()).is_some_and(release_id) {
                let installed = self.completed_release(directory.file_name().and_then(|name| name.to_str()).unwrap_or(""))?;
                return Ok(Some(Executable { executable_path, harness_path, source, ..installed }));
            }
        }
        Ok(Some(Executable { executable_path, harness_path, source, version: None, managed_version_directory: None }))
    }
    fn path_candidates(&self, binary: &str, environment: &HashMap<String, String>) -> Vec<PathBuf> {
        let value = environment.get("PATH").cloned().unwrap_or_default();
        std::env::split_paths(&value).filter(|directory| !directory.as_os_str().is_empty()).map(|directory| directory.join(binary)).collect()
    }
    /// The executable a binary path setting selects: a custom one, else the active managed release, else one on
    /// the `PATH`. `environment` is the environment the instance would run in.
    pub fn resolve(&self, binary_path: &str, environment: &HashMap<String, String>) -> R<Executable> {
        let resolve = || -> R<Executable> {
            let override_path = binary_path.trim();
            if !override_path.is_empty() {
                let candidates = if Path::new(override_path).is_absolute() || override_path.contains('/') || override_path.contains('\\') {
                    vec![std::path::absolute(override_path)?]
                } else {
                    self.path_candidates(override_path, environment)
                };
                for candidate in candidates {
                    if let Some(selected) = self.from_external(&candidate, "override")? {
                        return Ok(selected);
                    }
                }
                return Err(InstallError::new("resolve", "The custom Antigravity executable or its localharness_external sibling is missing or not executable."));
            }
            if self.active_path().exists() {
                let active: ActiveRelease = self.read_record(&self.active_path())?;
                return self.completed_release(&active.release_id);
            }
            for candidate in self.path_candidates(names(self.0.platform).0, environment) {
                if let Some(selected) = self.from_external(&candidate, "path")? {
                    return Ok(selected);
                }
            }
            Err(InstallError::new(
                "resolve",
                &match &self.0.asset {
                    Some(_) => "Antigravity is not installed. Install it in this environment or set a custom executable path.".to_owned(),
                    None => format!("Google does not publish an Antigravity runtime for {}-{}. Use a supported environment or a custom executable.", self.0.platform, self.0.arch),
                },
            ))
        };
        resolve().map_err(|error| error.or("resolve", "Could not read the Antigravity installation. Reinstall it or set a custom executable path."))
    }
    /// `resolve`, holding the managed release in use until the lease is dropped.
    pub async fn acquire(&self, binary_path: &str, environment: &HashMap<String, String>) -> R<Lease> {
        let _gate = self.0.gate.lock().await;
        let executable = self.resolve(binary_path, environment)?;
        if let Some(directory) = &executable.managed_version_directory {
            *self.0.leases.lock().unwrap().entry(directory.clone()).or_insert(0) += 1;
        }
        Ok(Lease { installation: self.clone(), directory: executable.managed_version_directory.clone(), executable })
    }

    // ---- install ----

    async fn validate(&self, executable: &Executable, version: &str) -> R<()> {
        let check = async {
            let initialized = Box::pin(crate::antigravity_instance::identify(executable)).await.map_err(|_| InstallError::raw())?;
            let capabilities = &initialized["agentCapabilities"];
            let info = &initialized["agentInfo"];
            // Antigravity 1.1.1 can report 2 with the legacy ACP response shape.
            let protocol = initialized["protocolVersion"].as_i64();
            if info["name"] != "antigravity-acp"
                || info["version"] != version
                || !matches!(protocol, Some(1 | 2))
                || capabilities["loadSession"] != true
                || capabilities["sessionCapabilities"]["resume"].is_null()
                || capabilities["auth"]["logout"].is_null()
                || !initialized["authMethods"].as_array().is_some_and(|methods| methods.iter().any(|method| method["id"] == "oauth-personal"))
            {
                return Err(InstallError::new("verify", "The downloaded runtime did not identify as the expected Google Antigravity release."));
            }
            Ok(())
        };
        tokio::time::timeout(Duration::from_secs(90), check)
            .await
            .map_err(|_| InstallError::raw())?
            .map_err(|error| error.or("verify", "The downloaded Antigravity runtime could not start in this environment."))
    }
    /// Point `active.json` at an installed release. Synchronous on purpose: a cancellation cannot land halfway.
    fn activate(&self, release: &ReleaseAsset) -> R<()> {
        let activated = (|| -> R<()> {
            let temporary = Staging::create(&self.0.managed, "active.json.")?;
            let pointer = temporary.0.join("contents.tmp");
            write_new(&pointer, json!({"releaseId":release.sha256}).to_string().as_bytes())?;
            std::fs::rename(&pointer, self.active_path())?;
            Ok(())
        })();
        activated.map_err(|error| error.or("activate", "Could not activate Antigravity. The previous runtime is unchanged. Check for locked files and try again."))?;
        self.update(|state| {
            state["phase"] = json!("succeeded");
            state["installedVersion"] = json!(release.version);
            state["message"] = Value::Null;
        });
        Ok(())
    }
    async fn download(&self, release: &ReleaseAsset, archive: &Path) -> R<()> {
        use tokio::io::AsyncWriteExt;
        let client = reqwest::Client::builder()
            // Never follow a redirect from https down to plain http, and never off the host asked for.
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                let first = attempt.previous().first().cloned();
                let secure_origin = first.as_ref().is_some_and(|first| first.scheme() == "https");
                let same_host = first.as_ref().is_some_and(|first| first.host_str() == attempt.url().host_str());
                if attempt.previous().len() > 5 || (secure_origin && attempt.url().scheme() != "https") || !same_host { attempt.stop() } else { attempt.follow() }
            }))
            .build()?;
        let mut response = client.get(&release.url).send().await?.error_for_status()?;
        // dl.google.com gzips the archive when the client accepts it, so `content-length` is then the encoded size;
        // the stream is checked against the pinned byte count and hash below either way.
        let identity = response.headers().get("content-encoding").and_then(|value| value.to_str().ok()).is_none_or(|value| value.trim().eq_ignore_ascii_case("identity"));
        let length = response.headers().get("content-length").and_then(|value| value.to_str().ok()).and_then(|value| value.parse::<u64>().ok());
        if identity && length.is_some_and(|length| length != release.archive_bytes) {
            return Err(InstallError::new("download", "The Antigravity download size did not match the pinned release."));
        }
        let mut file = {
            let mut options = tokio::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            options.open(archive).await?
        };
        let (mut hash, mut downloaded) = (Sha256::new(), 0u64);
        let mut last_progress = tokio::time::Instant::now();
        while let Some(chunk) = response.chunk().await? {
            downloaded += chunk.len() as u64;
            if downloaded > release.archive_bytes {
                return Err(InstallError::new("download", "The Antigravity download exceeded the pinned release size."));
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
            return Err(InstallError::new("download", "The Antigravity download failed its size or SHA-256 check. Nothing was installed."));
        }
        Ok(())
    }
    async fn install(&self, release: &ReleaseAsset) -> R<()> {
        let versions = self.versions();
        std::fs::create_dir_all(&versions)?;
        self.update(|state| state["canRemove"] = json!(true));
        let destination = versions.join(&release.sha256);
        if destination.exists() {
            let existing = self.completed_release(&release.sha256)?;
            if existing.version.as_deref() != Some(release.version.as_str()) {
                return Err(InstallError::new("verify", "The existing managed release has the wrong version. Remove it before reinstalling."));
            }
            self.update(|state| {
                state["phase"] = json!("verifying");
                state["message"] = json!("Checking the installed runtime.");
            });
            self.validate(&existing, &release.version).await?;
            return self.activate(release);
        }
        let required = release.archive_bytes + release.executable.bytes + release.harness.bytes + FREE_SPACE_MARGIN;
        if free_bytes(&versions).is_some_and(|available| available < required) {
            return Err(InstallError::new("download", &format!("Antigravity needs at least {} MiB of free space to install.", required.div_ceil(1024 * 1024))));
        }
        let staging = Staging::create(&versions, ".install-")?;
        let archive = staging.0.join("download.zip");
        let pair = staging.0.join("runtime");
        std::fs::create_dir(&pair)?;
        tokio::time::timeout(Duration::from_secs(45 * 60), self.download(release, &archive)).await.map_err(|_| InstallError::raw())??;
        self.update(|state| {
            state["phase"] = json!("extracting");
            state["message"] = json!("Extracting the verified runtime.");
        });
        {
            let (release, archive, pair) = (release.clone(), archive.clone(), pair.clone());
            tokio::task::spawn_blocking(move || extract_pair(&archive, &release, &pair)).await.map_err(|_| InstallError::raw())??;
        }
        std::fs::remove_file(&archive)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for name in [&release.executable.name, &release.harness.name] {
                std::fs::set_permissions(pair.join(name), std::fs::Permissions::from_mode(0o755))?;
            }
        }
        self.update(|state| {
            state["phase"] = json!("verifying");
            state["message"] = json!("Checking the downloaded runtime.");
        });
        let candidate = Executable { executable_path: pair.join(&release.executable.name), harness_path: pair.join(&release.harness.name), source: "managed", version: Some(release.version.clone()), managed_version_directory: Some(pair.clone()) };
        self.validate(&candidate, &release.version).await?;
        let record = json!({"releaseId":release.sha256,"version":release.version,"executable":{"name":release.executable.name,"bytes":release.executable.bytes},"harness":{"name":release.harness.name,"bytes":release.harness.bytes}});
        write_new(&pair.join(RELEASE_RECORD), record.to_string().as_bytes())?;
        if let Err(cause) = std::fs::rename(&pair, &destination) {
            // Another installation published the same release first: it is used if it is complete and right.
            let existing = self.completed_release(&release.sha256).map_err(|_| InstallError::new("activate", "Could not publish the Antigravity runtime. The previous release is unchanged. Try again."))?;
            let _ = cause;
            if existing.version.as_deref() != Some(release.version.as_str()) {
                return Err(InstallError::new("activate", "Another installation published a different Antigravity release."));
            }
            self.validate(&existing, &release.version).await?;
        }
        // The archive is no longer needed; it is gone before clients are told the install is done.
        drop(staging);
        self.activate(release)
    }
    pub async fn start(&self) -> Result<Value, InstallError> {
        let _gate = self.0.gate.lock().await;
        let failure = |error: InstallError| error.or("start", "Could not start the Antigravity installation.");
        let current = self.state();
        if is_running(&current) {
            return Ok(current);
        }
        let Some(asset) = self.0.asset.clone() else {
            return Err(InstallError::new("start", &format!("Google does not publish an Antigravity runtime for {}-{}. Use a supported remote environment or a custom executable.", self.0.platform, self.0.arch)));
        };
        // A download goes to Google's release host only; other sources need code that supplies them.
        if !self.0.trusted && !asset.url.starts_with(OFFICIAL_RELEASE_PREFIX) {
            return Err(failure(InstallError::raw()));
        }
        let operation_id = uuid::Uuid::new_v4().to_string();
        let next = json!({"driver":"antigravity","operationId":operation_id,"phase":"downloading","downloadedBytes":0,"totalBytes":asset.archive_bytes,
            "version":asset.version,"installedVersion":current["installedVersion"],"canRemove":current["canRemove"],"message":"Downloading Google's official Antigravity runtime."});
        self.replace(next.clone());
        let installation = self.clone();
        let operation = operation_id.clone();
        let task = tokio::spawn(async move {
            let result = Box::pin(installation.install(&asset)).await;
            if let Err(error) = result {
                let error = error.or("install", "Could not install Antigravity. Check free disk space and directory access, then try again.");
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
        let default = "Could not remove the managed Antigravity runtime. Check for open processes and try again.";
        if is_running(&self.state()) || !self.0.leases.lock().unwrap().is_empty() {
            return Err(InstallError::new("remove", "Stop Antigravity sessions and sign-in flows before removing its managed runtime."));
        }
        if let Ok(managed) = std::fs::canonicalize(&self.0.managed) {
            for binary in protected.iter().filter(|binary| !binary.trim().is_empty()) {
                let inside = |candidate: &Path| candidate.starts_with(&managed);
                let resolved = self.resolve(binary, &std::env::vars().collect()).ok();
                let candidate = std::fs::canonicalize(binary).unwrap_or_else(|_| std::path::absolute(binary).unwrap_or_else(|_| PathBuf::from(binary)));
                if resolved.is_some_and(|selected| selected.managed_version_directory.is_some()) || inside(&candidate) {
                    return Err(InstallError::new("remove", "A provider instance has a custom path inside this managed runtime. Clear that path before removing it."));
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
            state["canRemove"] = json!(false);
            state["message"] = Value::Null;
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn the_pinned_release_table_matches_the_source() {
        let fixture: Value = serde_json::from_str(include_str!("../tests/fixtures/antigravity-pure.json")).unwrap();
        for case in fixture["releases"].as_array().unwrap() {
            let asset = release_asset(case["platform"].as_str().unwrap(), case["arch"].as_str().unwrap()).map_or(Value::Null, |asset| json!({
                "version":asset.version,"url":asset.url,"sha256":asset.sha256,"archiveBytes":asset.archive_bytes,
                "executable":{"name":asset.executable.name,"bytes":asset.executable.bytes},"harness":{"name":asset.harness.name,"bytes":asset.harness.bytes}}));
            assert_eq!(asset, case["asset"], "{} {}", case["platform"], case["arch"]);
            if !asset.is_null() {
                assert!(asset["url"].as_str().unwrap().starts_with(OFFICIAL_RELEASE_PREFIX));
            }
        }
    }
}
