//! The installer against a server on loopback that serves a small archive. Nothing here reaches a real host, and the pinned release is only read.
use super::*;
use crate::test_support::HANG_BOUND;
use axum::{Router, body::Body, extract::State, http::header, response::Response, routing::get};
use sha2::Digest;
use std::sync::atomic::AtomicUsize;
use tokio::sync::Notify;

const ROOT: &str = "chrome-headless-shell-fixture/";
const EXECUTABLE: &str = "chrome-headless-shell";

struct Member<'a> {
    name: &'a str,
    data: &'a [u8],
    mode: u32,
    flags: u16,
}
fn member<'a>(name: &'a str, data: &'a [u8], mode: u32) -> Member<'a> {
    Member { name, data, mode, flags: 0 }
}

/// A zip with every member stored, the way a fixture needs it.
fn zip(members: &[Member]) -> Vec<u8> {
    let (mut records, mut directory) = (Vec::new(), Vec::new());
    for member in members {
        let offset = records.len() as u32;
        let mut crc = flate2::Crc::new();
        crc.update(member.data);
        let (name, size) = (member.name.as_bytes(), member.data.len() as u32);
        records.extend_from_slice(&0x04034b50u32.to_le_bytes());
        for field in [20u16, member.flags, 0, 0, 0] {
            records.extend_from_slice(&field.to_le_bytes());
        }
        for field in [crc.sum(), size, size] {
            records.extend_from_slice(&field.to_le_bytes());
        }
        records.extend_from_slice(&(name.len() as u16).to_le_bytes());
        records.extend_from_slice(&0u16.to_le_bytes());
        records.extend_from_slice(name);
        records.extend_from_slice(member.data);
        directory.extend_from_slice(&0x02014b50u32.to_le_bytes());
        for field in [(3u16 << 8) | 20, 20, member.flags, 0, 0, 0] {
            directory.extend_from_slice(&field.to_le_bytes());
        }
        for field in [crc.sum(), size, size] {
            directory.extend_from_slice(&field.to_le_bytes());
        }
        for field in [name.len() as u16, 0, 0, 0, 0] {
            directory.extend_from_slice(&field.to_le_bytes());
        }
        directory.extend_from_slice(&(member.mode << 16).to_le_bytes());
        directory.extend_from_slice(&offset.to_le_bytes());
        directory.extend_from_slice(name);
    }
    let mut archive = records;
    let directory_offset = archive.len() as u32;
    archive.extend_from_slice(&directory);
    archive.extend_from_slice(&0x06054b50u32.to_le_bytes());
    for field in [0u16, 0, members.len() as u16, members.len() as u16] {
        archive.extend_from_slice(&field.to_le_bytes());
    }
    archive.extend_from_slice(&(directory.len() as u32).to_le_bytes());
    archive.extend_from_slice(&directory_offset.to_le_bytes());
    archive.extend_from_slice(&0u16.to_le_bytes());
    archive
}

fn browser_archive() -> Vec<u8> {
    zip(&[
        member(ROOT, b"", 0o40755),
        member(&format!("{ROOT}{EXECUTABLE}"), b"#!/bin/sh\n", 0o100755),
        member(&format!("{ROOT}libEGL.so"), b"library", 0o100755),
        member(&format!("{ROOT}locales/"), b"", 0o40755),
        member(&format!("{ROOT}locales/en-US.pak"), b"strings", 0o100644),
    ])
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

/// What the loopback server sends: its chunks in order, optionally holding the stream after the first until released.
struct Served {
    chunks: Vec<Vec<u8>>,
    content_length: Option<usize>,
    hold_after_first: Option<(Arc<Notify>, Arc<Notify>)>,
    requests: AtomicUsize,
}

async fn serve(served: Arc<Served>) -> String {
    async fn handler(State(served): State<Arc<Served>>) -> Response {
        served.requests.fetch_add(1, Ordering::SeqCst);
        let stream = futures_util::stream::unfold((0usize, served.clone()), |(next, served)| async move {
            let chunk = served.chunks.get(next)?.clone();
            if next == 1 {
                if let Some((_, release)) = &served.hold_after_first {
                    release.notified().await;
                }
            }
            if next == 0 {
                if let Some((first_sent, _)) = &served.hold_after_first {
                    first_sent.notify_one();
                }
            }
            Some((Ok::<_, std::io::Error>(axum::body::Bytes::from(chunk)), (next + 1, served)))
        });
        let mut response = Response::builder();
        if let Some(length) = served.content_length {
            response = response.header(header::CONTENT_LENGTH, length);
        }
        response.body(Body::from_stream(stream)).unwrap()
    }
    let app = Router::new().route("/chrome.zip", get(handler)).with_state(served);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{address}/chrome.zip")
}

struct Harness {
    browser: PreviewBrowser,
    served: Arc<Served>,
    install_root: PathBuf,
    base: tempfile::TempDir,
}

async fn harness(archive: Vec<u8>, sha: Option<String>, chunks: Option<Vec<Vec<u8>>>, content_length: Option<usize>, hold: Option<(Arc<Notify>, Arc<Notify>)>) -> Harness {
    let base = tempfile::tempdir().unwrap();
    let chunks = chunks.unwrap_or_else(|| vec![archive[..40].to_vec(), archive[40..].to_vec()]);
    let served = Arc::new(Served { chunks, content_length, hold_after_first: hold, requests: AtomicUsize::new(0) });
    let url = serve(served.clone()).await;
    let mut options = PreviewBrowserOptions::new(base.path().to_path_buf());
    options.release = Some(Some(PreviewBrowserRelease { version: "1.2.3".into(), platform: "fixture".into(), url, bytes: archive.len() as u64, sha256: sha.unwrap_or_else(|| sha256(&archive)) }));
    options.host = Some(("linux".into(), "x64".into()));
    let install_root = base.path().join("tools").join("chrome-headless-shell").join("fixture");
    Harness { browser: PreviewBrowser::new(options), served, install_root, base }
}

fn names(directory: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(directory).map(|entries| entries.map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
    names.sort();
    names
}

#[tokio::test]
async fn a_verified_download_installs_with_its_file_modes_and_is_then_reused() {
    let h = harness(browser_archive(), None, None, None, None).await;
    // An older build, another server's install in progress, and one abandoned two hours ago.
    std::fs::create_dir_all(h.install_root.join("1.0.0")).unwrap();
    std::fs::create_dir_all(h.install_root.join(".install-other")).unwrap();
    let abandoned = h.install_root.join(".install-abandoned");
    std::fs::create_dir_all(&abandoned).unwrap();
    std::fs::File::open(&abandoned).unwrap().set_modified(std::time::SystemTime::now() - Duration::from_secs(2 * 3600)).unwrap();
    assert_eq!(h.browser.installed(), None);

    let executable = tokio::time::timeout(HANG_BOUND, h.browser.executable()).await.unwrap().unwrap();

    let version = h.install_root.join("1.2.3");
    assert_eq!(executable, version.join(EXECUTABLE));
    assert_eq!(std::fs::read_to_string(version.join("locales").join("en-US.pak")).unwrap(), "strings");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |file: &str| std::fs::metadata(version.join(file)).unwrap().permissions().mode() & 0o777;
        assert_eq!((mode(EXECUTABLE), mode("libEGL.so"), mode("locales/en-US.pak")), (0o755, 0o755, 0o644));
    }
    // The old build and stale staging are gone; a fresh staging directory stays.
    assert_eq!(names(&h.install_root), [".install-other", "1.2.3"]);
    assert_eq!(h.browser.executable().await.unwrap(), executable);
    assert_eq!(h.browser.installed(), Some(executable));
    assert_eq!(h.served.requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_download_that_is_not_the_pinned_release_installs_nothing_and_is_retried() {
    let archive = browser_archive();
    let cases: Vec<(&str, Harness)> = vec![
        ("hash", harness(archive.clone(), Some("0".repeat(64)), None, None, None).await),
        ("short", harness(archive.clone(), None, Some(vec![archive[..archive.len() - 1].to_vec()]), None, None).await),
        ("oversized", harness(archive.clone(), None, Some(vec![archive.clone(), b"extra".to_vec()]), None, None).await),
        // A length the server states up front is checked before anything is written.
        ("stated length", harness(archive.clone(), None, Some(vec![archive.clone(), b"extra".to_vec()]), Some(archive.len() + 5), None).await),
    ];
    for (name, h) in cases {
        let error = tokio::time::timeout(HANG_BOUND, h.browser.executable()).await.unwrap().unwrap_err();
        assert_eq!(error.tag(), "PreviewBrowserInstallError", "{name}");
        let message = error.message();
        assert!(message.starts_with("T3 could not install its headless browser: ") && message.ends_with(" Try again."), "{name}: {message}");
        assert_eq!(names(&h.install_root), Vec::<String>::new(), "{name}: nothing is left behind");
        // The failure was reported once; the next call tries again.
        tokio::time::timeout(HANG_BOUND, h.browser.executable()).await.unwrap().unwrap_err();
        assert_eq!(h.served.requests.load(Ordering::SeqCst), 2, "{name}");
    }
}

#[tokio::test]
async fn an_archive_with_an_unsafe_or_unexpected_member_is_refused_before_anything_escapes() {
    let exe = format!("{ROOT}{EXECUTABLE}");
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("a parent-directory entry", zip(&[member(&exe, b"#!/bin/sh\n", 0o100755), member(&format!("{ROOT}../escape"), b"escaped", 0o100644)])),
        ("an absolute entry", zip(&[member(&exe, b"#!/bin/sh\n", 0o100755), member("/escape", b"escaped", 0o100644)])),
        ("an entry outside the top directory", zip(&[member(&exe, b"#!/bin/sh\n", 0o100755), member("escape", b"escaped", 0o100644)])),
        ("a backslash entry", zip(&[member(&exe, b"#!/bin/sh\n", 0o100755), member(&format!("{ROOT}a\\..\\b"), b"x", 0o100644)])),
        ("a symbolic link", zip(&[member(&exe, b"#!/bin/sh\n", 0o100755), member(&format!("{ROOT}link"), b"/etc/passwd", 0o120777)])),
        ("a device", zip(&[member(&exe, b"#!/bin/sh\n", 0o100755), member(&format!("{ROOT}dev"), b"", 0o020666)])),
        ("an encrypted entry", zip(&[member(&exe, b"#!/bin/sh\n", 0o100755), Member { name: &format!("{ROOT}secret"), data: b"x", mode: 0o100644, flags: 1 }])),
        ("no shell", zip(&[member(&format!("{ROOT}libEGL.so"), b"library", 0o100755)])),
    ];
    for (name, archive) in cases {
        let h = harness(archive, None, None, None, None).await;
        let error = tokio::time::timeout(HANG_BOUND, h.browser.executable()).await.unwrap().unwrap_err();
        assert_eq!(error.tag(), "PreviewBrowserInstallError", "{name}");
        assert_eq!(names(&h.install_root), Vec::<String>::new(), "{name}");
        // Nothing was written beside the install root either.
        assert_eq!(names(h.install_root.parent().unwrap()), ["fixture"], "{name}");
        assert_eq!(names(h.base.path()), ["tools"], "{name}");
    }
}

#[tokio::test]
async fn a_member_that_lies_about_its_size_or_checksum_is_refused() {
    let exe = format!("{ROOT}{EXECUTABLE}");
    let mut archive = zip(&[member(&exe, b"#!/bin/sh\n", 0o100755)]);
    // Flip a byte of the stored data: the checksum the archive records no longer matches.
    let at = archive.windows(10).position(|window| window == b"#!/bin/sh\n").unwrap();
    archive[at] = b'X';
    let h = harness(archive, None, None, None, None).await;
    let error = tokio::time::timeout(HANG_BOUND, h.browser.executable()).await.unwrap().unwrap_err();
    assert_eq!(error.tag(), "PreviewBrowserInstallError");
    assert_eq!(names(&h.install_root), Vec::<String>::new());
}

/// The one test with a real clock: the download is real traffic on loopback, which a paused clock cannot be mixed with (it advances whenever the
/// runtime waits on the network, and would end the download's own time limit). The callers' wait is short, and the server holds the download until
/// the test lets it go, so they give up whatever the machine's speed.
#[tokio::test]
async fn callers_share_one_install_which_finishes_after_they_stop_waiting() {
    let archive = browser_archive();
    let (first_sent, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let h = harness(archive.clone(), None, Some(vec![archive[..100].to_vec(), archive[100..].to_vec()]), None, Some((first_sent.clone(), release.clone()))).await;
    let wait = Duration::from_millis(100);
    let (a, b) = (h.browser.clone(), h.browser.clone());
    let callers = [tokio::spawn(async move { a.executable_within(wait).await }), tokio::spawn(async move { b.executable_within(wait).await })];
    tokio::time::timeout(HANG_BOUND, first_sent.notified()).await.unwrap();
    for caller in callers {
        let error = caller.await.unwrap().unwrap_err();
        let PreviewBrowserError::Installing { downloaded_bytes, total_bytes, unpacking } = error else { panic!("{error:?}") };
        assert_eq!((total_bytes, unpacking), (archive.len() as u64, false));
        assert!(downloaded_bytes <= 100);
    }
    // The install belongs to the service: it outlives the callers that gave up on it, and a caller that comes back joins it.
    release.notify_one();
    let executable = h.browser.executable_within(HANG_BOUND).await.unwrap();
    assert!(executable.ends_with(EXECUTABLE));
    assert_eq!(h.served.requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_host_chrome_for_testing_does_not_build_for_is_reported_without_downloading() {
    let base = tempfile::tempdir().unwrap();
    let mut options = PreviewBrowserOptions::new(base.path().to_path_buf());
    options.host = Some(("freebsd".into(), "x64".into()));
    let browser = PreviewBrowser::new(options);
    let error = browser.executable().await.unwrap_err();
    assert_eq!(error, PreviewBrowserError::Unsupported { platform: "freebsd".into(), arch: "x64".into() });
    assert_eq!(error.message(), "T3's headless browser is not available on freebsd-x64: Chrome for Testing has no headless shell for it.");
    assert_eq!(browser.installed(), None);
    assert_eq!(names(base.path()), Vec::<String>::new(), "nothing was created");
}

#[test]
fn one_chrome_for_testing_build_is_pinned_per_host() {
    let hosts = [("linux", "x64", "linux64"), ("linux", "arm64", "linux-arm64"), ("darwin", "arm64", "mac-arm64"), ("darwin", "x64", "mac-x64"), ("win32", "x64", "win64"), ("win32", "arm64", "win64"), ("win32", "ia32", "win32")];
    let mut seen = std::collections::BTreeSet::new();
    for (platform, arch, chrome) in hosts {
        let release = preview_browser_release(platform, arch).unwrap_or_else(|| panic!("{platform}-{arch}"));
        assert_eq!((release.version.as_str(), release.platform.as_str()), ("154.0.8037.92", chrome));
        assert_eq!(release.url, format!("https://storage.googleapis.com/chrome-for-testing-public/154.0.8037.92/{chrome}/chrome-headless-shell-{chrome}.zip"));
        assert!(release.bytes > 50_000_000 && release.sha256.len() == 64 && release.sha256.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()), "{chrome}");
        seen.insert(release.sha256);
    }
    // Windows on Arm runs the x64 build, so six builds serve seven hosts.
    assert_eq!(seen.len(), 6);
    for (platform, arch) in [("freebsd", "x64"), ("linux", "ia32"), ("darwin", "ia32"), ("win32", "mips")] {
        assert_eq!(preview_browser_release(platform, arch), None, "{platform}-{arch}");
    }
}

#[test]
fn the_agent_is_told_how_far_the_install_has_come() {
    let downloading = PreviewBrowserError::Installing { downloaded_bytes: 34_600_000, total_bytes: 120_477_194, unpacking: false };
    assert_eq!(downloading.message(), "T3 is installing its headless browser (35 of 120 MB downloaded). Try again in a minute.");
    let unpacking = PreviewBrowserError::Installing { downloaded_bytes: 120_477_194, total_bytes: 120_477_194, unpacking: true };
    assert_eq!(unpacking.message(), "T3 is installing its headless browser (unpacking). Try again in a minute.");
    assert_eq!(unpacking.wire(), json!({"_tag":"PreviewBrowserInstallingError","downloadedBytes":120477194,"totalBytes":120477194,"unpacking":true}));
}

#[test]
fn this_host_is_named_in_nodes_terms() {
    let (platform, arch) = host();
    #[cfg(target_os = "macos")]
    assert_eq!(platform, "darwin");
    #[cfg(target_os = "linux")]
    assert_eq!(platform, "linux");
    #[cfg(target_os = "windows")]
    assert_eq!(platform, "win32");
    #[cfg(target_arch = "aarch64")]
    assert_eq!(arch, "arm64");
    #[cfg(target_arch = "x86_64")]
    assert_eq!(arch, "x64");
    let _ = (platform, arch);
}
