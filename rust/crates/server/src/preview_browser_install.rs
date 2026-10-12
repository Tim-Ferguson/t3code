//! The one browser T3 installs on a host (apps/server/src/preview/PreviewBrowser.ts): a pinned Chrome for Testing headless shell, downloaded
//! once into `<home>/tools/chrome-headless-shell/<platform>/<version>/`. HTML render previews and server browser tabs both run it; neither
//! uses a browser the user installed.
//!
//! The archive is pinned by size and SHA-256, checked as it downloads, and unpacked only after it matches. It is unpacked into a staging
//! directory beside its destination and published by one rename, so a runnable binary in the version directory is the mark of a complete
//! install. To bump the pin, take a version from
//! https://googlechromelabs.github.io/chrome-for-testing/known-good-versions-with-downloads.json, download each platform's
//! chrome-headless-shell zip, and replace the version and every byte count and SHA-256 below. Hosts drop the old build after the new one installs.
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::watch;

const VERSION: &str = "154.0.8037.92";

/// Size and SHA-256 of each platform's archive.
const ARCHIVES: &[(&str, u64, &str)] = &[
    ("linux64", 120_477_194, "636aa5c79f2693632e9921b8bbb050038ba11672e02346c06c20f991aed096f9"),
    ("linux-arm64", 121_182_296, "0ed0e47d9e9f639197f508d62ada09e5c6b4c4c60edab3160a9312a733091df6"),
    ("mac-arm64", 99_221_129, "77da14e75d7f2568e6f7898d3df7cdc6faac74b15e903b2c9d486ebb6ca9b929"),
    ("mac-x64", 104_748_425, "a54292aaacbb77f76f6ef47558e7c51ab884044e0adacca315567f83c060bcc4"),
    ("win32", 114_295_943, "56b30d2d6c35775ebf8dc3618680f6529e1c38c87f7feb28a16e9904273d51f7"),
    ("win64", 120_822_223, "3ac2561f02d9d87aadc0399d00b9002d718a4c365624fa67db9e7bfaf6b1a568"),
];

/// Chrome for Testing's name for a host: Node's platform names (`linux`, `darwin`, `win32`) and architectures (`x64`, `arm64`, `ia32`).
fn chrome_platform(platform: &str, arch: &str) -> Option<&'static str> {
    match (platform, arch) {
        ("linux", "x64") => Some("linux64"),
        ("linux", "arm64") => Some("linux-arm64"),
        ("darwin", "arm64") => Some("mac-arm64"),
        ("darwin", "x64") => Some("mac-x64"),
        // There is no Windows arm64 build; Windows on Arm runs the x64 one under emulation.
        ("win32", "ia32") => Some("win32"),
        ("win32", "x64" | "arm64") => Some("win64"),
        _ => None,
    }
}

/// This host in Node's names, which the pin and the install directory are spelled in.
pub fn host() -> (&'static str, &'static str) {
    let platform = match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        other => other,
    };
    (platform, arch)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewBrowserRelease {
    pub version: String,
    /// Chrome for Testing's platform name, which also names the archive's top directory.
    pub platform: String,
    pub url: String,
    pub bytes: u64,
    pub sha256: String,
}

/// The pinned release for a host, or none where Chrome for Testing builds no headless shell.
pub fn preview_browser_release(platform: &str, arch: &str) -> Option<PreviewBrowserRelease> {
    let chrome = chrome_platform(platform, arch)?;
    let (_, bytes, sha256) = ARCHIVES.iter().find(|(name, ..)| *name == chrome)?;
    Some(PreviewBrowserRelease {
        version: VERSION.into(),
        platform: chrome.into(),
        url: format!("https://storage.googleapis.com/chrome-for-testing-public/{VERSION}/{chrome}/chrome-headless-shell-{chrome}.zip"),
        bytes: *bytes,
        sha256: (*sha256).into(),
    })
}

/// Why the browser is not available: the install failed, is still running, or this host has no build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewBrowserError {
    Install { detail: String },
    Installing { downloaded_bytes: u64, total_bytes: u64, unpacking: bool },
    Unsupported { platform: String, arch: String },
}

impl PreviewBrowserError {
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Install { .. } => "PreviewBrowserInstallError",
            Self::Installing { .. } => "PreviewBrowserInstallingError",
            Self::Unsupported { .. } => "PreviewBrowserUnsupportedError",
        }
    }
    pub fn message(&self) -> String {
        match self {
            Self::Install { detail } => format!("T3 could not install its headless browser: {detail} Try again."),
            Self::Installing { downloaded_bytes, total_bytes, unpacking } => {
                let megabytes = |bytes: &u64| (*bytes as f64 / 1_000_000.0).round() as u64;
                let progress = if *unpacking { "unpacking".to_owned() } else { format!("{} of {} MB downloaded", megabytes(downloaded_bytes), megabytes(total_bytes)) };
                format!("T3 is installing its headless browser ({progress}). Try again in a minute.")
            }
            Self::Unsupported { platform, arch } => format!("T3's headless browser is not available on {platform}-{arch}: Chrome for Testing has no headless shell for it."),
        }
    }
    pub fn wire(&self) -> Value {
        match self {
            Self::Install { detail } => json!({"_tag":self.tag(),"detail":detail}),
            Self::Installing { downloaded_bytes, total_bytes, unpacking } => json!({"_tag":self.tag(),"downloadedBytes":downloaded_bytes,"totalBytes":total_bytes,"unpacking":unpacking}),
            Self::Unsupported { platform, arch } => json!({"_tag":self.tag(),"platform":platform,"arch":arch}),
        }
    }
}
impl std::fmt::Display for PreviewBrowserError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}
impl std::error::Error for PreviewBrowserError {}

fn failure(detail: &str) -> PreviewBrowserError {
    PreviewBrowserError::Install { detail: detail.into() }
}

pub struct PreviewBrowserOptions {
    /// The T3 home the browser installs under.
    pub base_dir: PathBuf,
    /// Overrides the pin: another build, or none (`Some(None)`) for a host Chrome for Testing does not build for. Tests point this at a server on
    /// loopback serving a small archive; nothing else changes where the browser comes from.
    pub release: Option<Option<PreviewBrowserRelease>>,
    /// The host's platform and architecture in Node's names. Defaults to this host.
    pub host: Option<(String, String)>,
    /// How long a caller waits on an install in progress. Provider tool calls time out near 60 seconds.
    pub wait: Duration,
}

impl PreviewBrowserOptions {
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir, release: None, host: None, wait: Duration::from_secs(45) }
    }
}

const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Longer than any install can run: the download times out at 15 minutes.
const STAGING_STALE_AFTER: Duration = Duration::from_secs(60 * 60);

type Outcome = Option<Result<PathBuf, PreviewBrowserError>>;

enum State {
    Idle,
    Installing(watch::Receiver<Outcome>),
    Installed(PathBuf),
    Failed(PreviewBrowserError),
}

struct Inner {
    release: Option<PreviewBrowserRelease>,
    platform: String,
    arch: String,
    install_root: PathBuf,
    executable_name: &'static str,
    wait: Duration,
    state: Mutex<State>,
    downloaded: AtomicU64,
    unpacking: AtomicBool,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// The pinned headless shell of one T3 home. Cloning shares it.
#[derive(Clone)]
pub struct PreviewBrowser(Arc<Inner>);

impl PreviewBrowser {
    pub fn new(options: PreviewBrowserOptions) -> Self {
        let (default_platform, default_arch) = host();
        let (platform, arch) = options.host.unwrap_or_else(|| (default_platform.into(), default_arch.into()));
        let release = options.release.unwrap_or_else(|| preview_browser_release(&platform, &arch));
        let install_root = options.base_dir.join("tools").join("chrome-headless-shell").join(release.as_ref().map_or_else(|| format!("{platform}-{arch}"), |release| release.platform.clone()));
        Self(Arc::new(Inner {
            release,
            executable_name: if platform == "win32" { "chrome-headless-shell.exe" } else { "chrome-headless-shell" },
            platform,
            arch,
            install_root,
            wait: options.wait,
            state: Mutex::new(State::Idle),
            downloaded: AtomicU64::new(0),
            unpacking: AtomicBool::new(false),
            task: Mutex::new(None),
        }))
    }

    /// Stops an install in progress, as the end of the server does.
    pub fn shutdown(&self) {
        if let Some(task) = self.0.task.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take() {
            task.abort();
        }
    }

    /// The installed headless shell, if any. Never starts or waits on an install.
    pub fn installed(&self) -> Option<PathBuf> {
        self.0.release.as_ref().and_then(|release| self.0.installed_executable(release))
    }

    /// Path to the installed headless shell. The first call starts the install; callers wait for it up to a bound and then get its progress
    /// instead, while the install keeps running.
    pub async fn executable(&self) -> Result<PathBuf, PreviewBrowserError> {
        self.executable_within(self.0.wait).await
    }

    /// `executable`, waiting at most `wait` on an install in progress.
    pub async fn executable_within(&self, wait: Duration) -> Result<PathBuf, PreviewBrowserError> {
        let Some(release) = self.0.release.clone() else {
            return Err(PreviewBrowserError::Unsupported { platform: self.0.platform.clone(), arch: self.0.arch.clone() });
        };
        let mut outcome = {
            let mut state = self.0.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            match &*state {
                State::Installed(path) => return Ok(path.clone()),
                // A failure is reported once, then cleared.
                State::Failed(error) => {
                    let error = error.clone();
                    *state = State::Idle;
                    return Err(error);
                }
                State::Installing(outcome) => outcome.clone(),
                State::Idle => {
                    if let Some(path) = self.0.installed_executable(&release) {
                        *state = State::Installed(path.clone());
                        return Ok(path);
                    }
                    self.0.downloaded.store(0, Ordering::Relaxed);
                    self.0.unpacking.store(false, Ordering::Relaxed);
                    let (done, outcome) = watch::channel(None);
                    *state = State::Installing(outcome.clone());
                    // The install belongs to the service, so it finishes even when no caller is still waiting.
                    let inner = self.0.clone();
                    let task = tokio::spawn(async move {
                        let result = inner.install(&release).await;
                        {
                            let mut state = inner.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                            *state = match &result {
                                Ok(path) => State::Installed(path.clone()),
                                Err(error) => State::Failed(error.clone()),
                            };
                        }
                        let _ = done.send(Some(result));
                    });
                    *self.0.task.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(task);
                    outcome
                }
            }
        };
        let finished = tokio::time::timeout(wait, outcome.wait_for(|outcome| outcome.is_some())).await;
        match finished {
            Ok(Ok(done)) => {
                let result = (*done).clone().expect("a finished install has an outcome");
                drop(done);
                if let Err(error) = &result {
                    // A waiter that saw the failure has reported it; the next call retries.
                    let mut state = self.0.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    if matches!(&*state, State::Failed(failed) if failed == error) {
                        *state = State::Idle;
                    }
                }
                result
            }
            Ok(Err(_)) => Err(failure("The install stopped.")),
            Err(_) => Err(PreviewBrowserError::Installing {
                downloaded_bytes: self.0.downloaded.load(Ordering::Relaxed),
                total_bytes: self.0.release.as_ref().map_or(0, |release| release.bytes),
                unpacking: self.0.unpacking.load(Ordering::Relaxed),
            }),
        }
    }
}

/// A directory removed with its contents when dropped: the staging area of an install, whichever way the install ends.
struct Staging(PathBuf);
impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Inner {
    /// The version directory appears by one rename of a fully unpacked tree, so a runnable binary there is the complete-install marker.
    fn installed_executable(&self, release: &PreviewBrowserRelease) -> Option<PathBuf> {
        let executable = self.install_root.join(&release.version).join(self.executable_name);
        let info = std::fs::metadata(&executable).ok()?;
        #[cfg(unix)]
        let runnable = {
            use std::os::unix::fs::PermissionsExt;
            info.permissions().mode() & 0o111 != 0
        };
        #[cfg(not(unix))]
        let runnable = true;
        (info.is_file() && (self.platform == "win32" || runnable)).then_some(executable)
    }

    async fn install(self: &Arc<Self>, release: &PreviewBrowserRelease) -> Result<PathBuf, PreviewBrowserError> {
        let saved = |cause: std::io::Error| PreviewBrowserError::Install { detail: format!("Could not save the browser in T3's home directory. ({cause})") };
        tokio::fs::create_dir_all(&self.install_root).await.map_err(saved)?;
        // Staging shares the install root's filesystem so publishing is one rename.
        let staging = self.install_root.join(format!(".install-{}", uuid::Uuid::new_v4().simple()));
        tokio::fs::create_dir(&staging).await.map_err(saved)?;
        let _staging = Staging(staging.clone());
        let archive = staging.join("download.zip");
        let unpacked = staging.join("browser");
        self.download(release, &archive).await?;
        self.unpacking.store(true, Ordering::Relaxed);
        let (executable_name, platform, root) = (self.executable_name, self.platform.clone(), format!("chrome-headless-shell-{}/", release.platform));
        let destination_dir = unpacked.clone();
        tokio::task::spawn_blocking(move || extract(&archive, &destination_dir, &root, executable_name, &platform))
            .await
            .map_err(|_| failure("Could not unpack the browser. Check free disk space in T3's home directory."))??;
        let destination = self.install_root.join(&release.version);
        // A directory left without a runnable binary would block the rename.
        match tokio::fs::remove_dir_all(&destination).await {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(saved(error)),
            _ => {}
        }
        tokio::fs::rename(&unpacked, &destination).await.map_err(saved)?;
        self.remove_old_builds(release).await;
        Ok(destination.join(self.executable_name))
    }

    /// Older builds and abandoned staging directories. Two servers can share a home, so a staging directory touched within the last hour may be
    /// another server's install in progress and stays.
    async fn remove_old_builds(&self, release: &PreviewBrowserRelease) {
        let Ok(mut entries) = tokio::fs::read_dir(&self.install_root).await else { return };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == release.version {
                continue;
            }
            if name.starts_with(".install-") {
                let modified = entry.metadata().await.ok().and_then(|info| info.modified().ok());
                if modified.is_none_or(|modified| modified.elapsed().is_ok_and(|age| age < STAGING_STALE_AFTER)) {
                    continue;
                }
            }
            if let Err(error) = tokio::fs::remove_dir_all(entry.path()).await {
                tracing::warn!(%name, %error, "Could not remove an old preview browser.");
            }
        }
    }

    async fn download(&self, release: &PreviewBrowserRelease, archive: &Path) -> Result<(), PreviewBrowserError> {
        let network = "The download from storage.googleapis.com failed. Check this machine's network access.";
        let attempt = async {
            let response = reqwest::Client::new().get(&release.url).send().await.and_then(|response| response.error_for_status()).map_err(|_| failure(network))?;
            let encoded = response.headers().get(reqwest::header::CONTENT_ENCODING).and_then(|value| value.to_str().ok()).map(|value| value.trim().to_ascii_lowercase());
            if encoded.is_none_or(|encoding| encoding == "identity") {
                if let Some(length) = response.content_length() {
                    if length != release.bytes {
                        return Err(failure("The download size did not match the pinned release."));
                    }
                }
            }
            let mut file = {
                let mut options = tokio::fs::OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                options.mode(0o600);
                options.open(archive).await.map_err(|_| failure(network))?
            };
            let mut hash = Sha256::new();
            let mut stream = response.bytes_stream();
            use futures_util::StreamExt;
            use tokio::io::AsyncWriteExt;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|_| failure(network))?;
                let total = self.downloaded.fetch_add(chunk.len() as u64, Ordering::Relaxed) + chunk.len() as u64;
                if total > release.bytes {
                    return Err(failure("The download was larger than the pinned release."));
                }
                hash.update(&chunk);
                file.write_all(&chunk).await.map_err(|_| failure(network))?;
            }
            file.flush().await.map_err(|_| failure(network))?;
            let hex: String = hash.finalize().iter().map(|byte| format!("{byte:02x}")).collect();
            if self.downloaded.load(Ordering::Relaxed) != release.bytes || hex != release.sha256 {
                return Err(failure("The download failed its size or SHA-256 check. Nothing was installed."));
            }
            Ok(())
        };
        tokio::time::timeout(DOWNLOAD_TIMEOUT, attempt).await.unwrap_or_else(|_| Err(failure(network)))
    }
}

// ---- the archive ----

/// One member of the zip's central directory.
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

fn corrupt() -> PreviewBrowserError {
    failure("The archive could not be read.")
}

/// A name a zip member may have: no backslash, not absolute, no drive, no `..` step (what the original's reader refuses).
fn strict_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !name.contains('\\') && !name.starts_with('/') && !(bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic()) && !name.split('/').any(|step| step == "..")
}

/// The central directory of a local, already verified archive. Zip64 is not what Google ships; a record that points past 32 bits is refused.
fn read_central_directory(file: &mut std::fs::File) -> Result<Vec<ZipEntry>, PreviewBrowserError> {
    let length = file.metadata().map_err(|_| corrupt())?.len();
    if length < 22 {
        return Err(corrupt());
    }
    let tail_length = length.min(22 + 65_535);
    file.seek(SeekFrom::Start(length - tail_length)).map_err(|_| corrupt())?;
    let mut tail = vec![0u8; tail_length as usize];
    file.read_exact(&mut tail).map_err(|_| corrupt())?;
    let end = (0..=tail.len() - 22).rev().find(|&at| tail[at..].starts_with(&[0x50, 0x4b, 0x05, 0x06])).ok_or_else(corrupt)?;
    let (count, size, offset) = (le16(&tail, end + 10) as usize, le32(&tail, end + 12) as u64, le32(&tail, end + 16) as u64);
    if le16(&tail, end + 8) as usize != count || count == 0xffff || size == 0xffff_ffff || offset == 0xffff_ffff || offset + size > length || size > 1 << 22 {
        return Err(corrupt());
    }
    file.seek(SeekFrom::Start(offset)).map_err(|_| corrupt())?;
    let mut directory = vec![0u8; size as usize];
    file.read_exact(&mut directory).map_err(|_| corrupt())?;
    let mut entries = Vec::with_capacity(count);
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

/// Writes one member to `destination`, never more than its recorded size, and checks it is exactly that long and matches its checksum.
fn extract_entry(file: &mut std::fs::File, entry: &ZipEntry, destination: &Path) -> Result<(), PreviewBrowserError> {
    let unpack = || failure("Could not unpack the browser. Check free disk space in T3's home directory.");
    file.seek(SeekFrom::Start(entry.header_offset)).map_err(|_| unpack())?;
    let mut local = [0u8; 30];
    file.read_exact(&mut local).map_err(|_| unpack())?;
    if local[..4] != [0x50, 0x4b, 0x03, 0x04] {
        return Err(unpack());
    }
    file.seek(SeekFrom::Current(le16(&local, 26) as i64 + le16(&local, 28) as i64)).map_err(|_| unpack())?;
    let source = file.take(entry.compressed);
    let mut reader: Box<dyn Read> = if entry.method == 0 { Box::new(source) } else { Box::new(flate2::read::DeflateDecoder::new(source)) };
    let mut output = {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(destination).map_err(|_| unpack())?
    };
    let (mut buffer, mut written, mut crc) = (vec![0u8; 1 << 20], 0u64, flate2::Crc::new());
    loop {
        let read = reader.read(&mut buffer).map_err(|_| unpack())?;
        if read == 0 {
            break;
        }
        written += read as u64;
        if written > entry.size {
            return Err(unpack());
        }
        crc.update(&buffer[..read]);
        output.write_all(&buffer[..read]).map_err(|_| unpack())?;
    }
    if written != entry.size || crc.sum() != entry.crc {
        return Err(unpack());
    }
    Ok(())
}

/// Unpacks the archive under `destination`, with its one top directory stripped. Every member must sit under that directory, name no parent step, be a plain file
/// or directory (no link, no device), be neither encrypted nor compressed with anything but deflate, and the shell itself must be among them.
fn extract(archive: &Path, destination: &Path, root: &str, executable_name: &str, platform: &str) -> Result<(), PreviewBrowserError> {
    let unsafe_entry = || failure("The archive contains an unexpected or unsafe entry.");
    let mut file = std::fs::File::open(archive).map_err(|_| failure("Could not open the verified archive."))?;
    let entries = read_central_directory(&mut file)?;
    std::fs::create_dir(destination).map_err(|_| failure("Could not unpack the browser. Check free disk space in T3's home directory."))?;
    let mut found_executable = false;
    for entry in &entries {
        if !strict_name(&entry.name) {
            return Err(corrupt());
        }
        let unix_mode = entry.external_attributes >> 16;
        let unix_type = unix_mode & 0o170000;
        let is_directory = entry.name.ends_with('/');
        let relative = entry.name.strip_prefix(root).ok_or_else(unsafe_entry)?;
        let target = destination.join(relative);
        if relative.split('/').any(|step| step == "..")
            || !target.starts_with(destination)
            || (unix_type != 0 && unix_type != if is_directory { 0o040000 } else { 0o100000 })
            || entry.flags & 1 != 0
            || ![0, 8].contains(&entry.method)
        {
            return Err(unsafe_entry());
        }
        if is_directory {
            std::fs::create_dir_all(&target).map_err(|_| failure("Could not unpack the browser. Check free disk space in T3's home directory."))?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|_| failure("Could not unpack the browser. Check free disk space in T3's home directory."))?;
        }
        extract_entry(&mut file, entry, &target)?;
        // The binary and its shared libraries carry exec bits in the archive.
        #[cfg(unix)]
        if platform != "win32" && unix_mode & 0o777 != 0 {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(unix_mode & 0o777)).map_err(|_| unsafe_entry())?;
        }
        #[cfg(not(unix))]
        let _ = platform;
        if relative == executable_name {
            found_executable = true;
        }
    }
    if found_executable { Ok(()) } else { Err(failure("The archive does not contain chrome-headless-shell.")) }
}

#[cfg(test)]
#[path = "preview_browser_install_tests.rs"]
mod tests;
