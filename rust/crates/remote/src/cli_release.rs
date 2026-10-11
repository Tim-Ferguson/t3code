//! Naming shared by the release workflow, the runtime installers and the install scripts for the
//! per-platform CLI archives attached to GitHub Releases (`packages/shared/src/cliRelease.ts`).
//! Every consumer derives the same file names from a version and a platform key, so a rename here
//! is a release-breaking change. The remote runner script ([`crate::ssh_scripts`]) builds the same
//! names in shell (`t3-<version>-<platform>-<arch>.tar.gz`).

use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;

use regex::Regex;

use crate::util::{JS_WHITESPACE_CLASS, js_trim};

pub const CLI_RELEASE_CHECKSUMS_FILE: &str = "SHA256SUMS";
/// Overrides the download origin for mirrors and air-gapped installs.
pub const CLI_RELEASE_BASE_URL_ENV: &str = "T3CODE_RELEASE_BASE_URL";
const CLI_RELEASE_REPOSITORY: &str = "pingdotgg/t3code";
pub const CLI_RELEASE_DEFAULT_BASE_URL: &str =
    "https://github.com/pingdotgg/t3code/releases/download";

/// The archives a release attaches. There is no darwin-x64: Node single-executables are unsupported
/// on x64 macOS and the binary segfaults on start.
pub const CLI_ARCHIVE_PLATFORM_KEYS: [&str; 5] = [
    "darwin-arm64",
    "linux-arm64",
    "linux-x64",
    "win32-arm64",
    "win32-x64",
];

/// `cliArchivePlatformKey`: `platform` is Node's (`darwin`, `linux`, `win32`), `arch` Node's
/// (`arm64`, `x64`).
pub fn cli_archive_platform_key(platform: &str, arch: &str) -> Option<&'static str> {
    let key = format!("{platform}-{arch}");
    CLI_ARCHIVE_PLATFORM_KEYS
        .iter()
        .copied()
        .find(|candidate| *candidate == key)
}

/// `cliArchiveTarCommand`: Windows ships bsdtar in System32, which reads both formats; a
/// Git-for-Windows GNU tar earlier on PATH cannot open the zip, so the system copy is named by
/// absolute path.
pub fn cli_archive_tar_command(platform: &str, env: &BTreeMap<String, String>) -> String {
    if platform != "win32" {
        return "tar".to_owned();
    }
    let system_root = env
        .get("SystemRoot")
        .or_else(|| env.get("windir"))
        .map(String::as_str)
        .unwrap_or("C:\\Windows");
    format!("{system_root}\\System32\\tar.exe")
}

/// `cliArchiveFileName`
pub fn cli_archive_file_name(version: &str, platform_key: &str) -> String {
    let extension = if platform_key.starts_with("win32") {
        "zip"
    } else {
        "tar.gz"
    };
    format!("t3-{version}-{platform_key}.{extension}")
}

/// `cliReleaseDownloadBaseUrl`: the directory `releases/download/<tag>/<asset>` lives under.
pub fn cli_release_download_base_url(version: &str, base_url: Option<&str>) -> String {
    let base = base_url
        .map(js_trim)
        .filter(|b| !b.is_empty())
        .unwrap_or(CLI_RELEASE_DEFAULT_BASE_URL);
    format!("{}/v{version}", base.trim_end_matches('/'))
}

/// `parseChecksums`: the `sha256sum`-style file attached to each release. Lines are
/// `<hex>  <file>`; a leading `*` marks binary mode and is ignored.
pub fn parse_checksums(text: &str) -> HashMap<String, String> {
    static LINE: OnceLock<Regex> = OnceLock::new();
    let line_pattern = LINE.get_or_init(|| {
        Regex::new(&format!(
            "^([0-9a-fA-F]{{64}})[{JS_WHITESPACE_CLASS}]+\\*?([^{JS_WHITESPACE_CLASS}][^\\n\\r\\u{{2028}}\\u{{2029}}]*)$"
        ))
        .expect("checksum line pattern")
    });
    let mut checksums = HashMap::new();
    for line in text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)) {
        if let Some(captures) = line_pattern.captures(js_trim(line)) {
            checksums.insert(captures[2].to_owned(), captures[1].to_lowercase());
        }
    }
    checksums
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliReleaseChannel {
    Stable,
    Nightly,
    Preview,
}

pub const CLI_RELEASE_CHANNELS: [CliReleaseChannel; 3] = [
    CliReleaseChannel::Stable,
    CliReleaseChannel::Nightly,
    CliReleaseChannel::Preview,
];

impl CliReleaseChannel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Nightly => "nightly",
            Self::Preview => "preview",
        }
    }
}

/// `cliReleaseChannelOf`: the release train a version was published on, derived from its
/// prerelease tag.
pub fn cli_release_channel_of(version: &str) -> CliReleaseChannel {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    let pattern = PATTERN.get_or_init(|| {
        Regex::new(r"^[^-+]+-(nightly|preview)\.[0-9]{8}\.[0-9]+$").expect("channel pattern")
    });
    match pattern.captures(version).map(|c| c[1].to_owned()).as_deref() {
        Some("nightly") => CliReleaseChannel::Nightly,
        Some("preview") => CliReleaseChannel::Preview,
        _ => CliReleaseChannel::Stable,
    }
}

/// `cliReleaseIndexPageUrl`: one page of GitHub's list-releases endpoint, newest first.
pub fn cli_release_index_page_url(page: u32) -> String {
    format!("https://api.github.com/repos/{CLI_RELEASE_REPOSITORY}/releases?per_page=100&page={page}")
}

/// One entry of GitHub's release list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseTag {
    pub tag_name: String,
    pub draft: bool,
}

/// `newestCliReleaseVersion`: the newest version on a channel from the release index. Tags are
/// `v<version>`; the channel is decided by the same rule the runtime uses, so a preview tag never
/// satisfies a nightly lookup and vice versa. Drafts are skipped because their assets are not
/// downloadable.
pub fn newest_cli_release_version(
    releases: &[ReleaseTag],
    channel: CliReleaseChannel,
) -> Option<String> {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    let pattern = PATTERN.get_or_init(|| {
        Regex::new(r"^v([0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?)$").expect("tag pattern")
    });
    releases
        .iter()
        .filter(|release| !release.draft)
        .filter_map(|release| pattern.captures(&release.tag_name).map(|c| c[1].to_owned()))
        .find(|version| cli_release_channel_of(version) == channel)
}
