#![cfg(unix)]
//! Managed Antigravity installation through the real WebSocket transport: downloads come from a loopback host
//! serving scripted archives (the pinned release table is substituted by code, which is the only way the official
//! host rule is waived), the machine's own PATH is never consulted, and no real Antigravity is ever run: the
//! "agent" in an archive is a script that records that it ran and then answers `initialize` like the real one.
//! The agent must never run for an archive that failed verification; the marker file proves it.
mod provider_manage_support;
use provider_manage_support::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use t3_server::antigravity_install::{AntigravityInstallation, File, InstallOptions, ReleaseAsset};

const AGENT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/antigravity-turn-agent.py");
const VERSION: &str = "1.3.0";

fn crc(bytes: &[u8]) -> u32 {
    let mut crc = flate2::Crc::new();
    crc.update(bytes);
    crc.sum()
}
/// A zip of stored members. `attributes` is each member's external file attributes.
fn zip(members: &[(&str, &[u8], u32)]) -> Vec<u8> {
    let (mut data, mut directory) = (vec![], vec![]);
    for (name, bytes, attributes) in members {
        let offset = data.len() as u32;
        let header = |signature: u32, central: bool| {
            let mut out = signature.to_le_bytes().to_vec();
            if central {
                out.extend(0x031eu16.to_le_bytes());
            }
            out.extend([20u16, 0, 0, 0, 0x21].iter().flat_map(|value| value.to_le_bytes()));
            out.extend(crc(bytes).to_le_bytes());
            out.extend((bytes.len() as u32).to_le_bytes());
            out.extend((bytes.len() as u32).to_le_bytes());
            out.extend((name.len() as u16).to_le_bytes());
            out.extend(0u16.to_le_bytes());
            out
        };
        data.extend(header(0x0403_4b50, false));
        data.extend(name.as_bytes());
        data.extend(*bytes);
        directory.extend(header(0x0201_4b50, true));
        directory.extend([0u16, 0, 0].iter().flat_map(|value| value.to_le_bytes()));
        directory.extend(attributes.to_le_bytes());
        directory.extend(offset.to_le_bytes());
        directory.extend(name.as_bytes());
    }
    let (start, size) = (data.len() as u32, directory.len() as u32);
    data.extend(directory);
    data.extend(0x0605_4b50u32.to_le_bytes());
    data.extend([0u16, 0, members.len() as u16, members.len() as u16].iter().flat_map(|value| value.to_le_bytes()));
    data.extend(size.to_le_bytes());
    data.extend(start.to_le_bytes());
    data.extend(0u16.to_le_bytes());
    data
}
const FILE_MODE: u32 = 0o100755 << 16;
/// The agent of an archive: it leaves a mark that it was started, then answers like the real agent.
fn agent_script(marker: &Path, exit_code: Option<i32>) -> Vec<u8> {
    match exit_code {
        Some(code) => format!("#!/bin/sh\necho ran >> {}\nexit {code}\n", marker.display()).into_bytes(),
        None => format!("#!/bin/sh\necho ran >> {}\nexec python3 {AGENT}\n", marker.display()).into_bytes(),
    }
}
fn executable_names() -> (&'static str, &'static str) {
    ("agy_acp_server.par", "localharness_external")
}
struct Archive {
    bytes: Vec<u8>,
    asset: ReleaseAsset,
}
fn archive(url: String, agent: &[u8], mutate: impl FnOnce(&mut Vec<(String, Vec<u8>, u32)>)) -> Archive {
    let (agent_name, harness_name) = executable_names();
    let harness = b"#!/bin/sh\nexit 0\n".to_vec();
    let mut members = vec![(agent_name.to_owned(), agent.to_vec(), FILE_MODE), (harness_name.to_owned(), harness.clone(), FILE_MODE)];
    mutate(&mut members);
    let borrowed: Vec<(&str, &[u8], u32)> = members.iter().map(|(name, bytes, attributes)| (name.as_str(), bytes.as_slice(), *attributes)).collect();
    let bytes = zip(&borrowed);
    let asset = ReleaseAsset {
        version: VERSION.into(),
        url,
        sha256: format!("{:x}", Sha256::digest(&bytes)),
        archive_bytes: bytes.len() as u64,
        executable: File { name: agent_name.into(), bytes: agent.len() as u64 },
        harness: File { name: harness_name.into(), bytes: harness.len() as u64 },
    };
    Archive { bytes, asset }
}
struct Fixture {
    _marker_directory: tempfile::TempDir,
    harness: Harness,
    releases: Releases,
    managed: PathBuf,
    marker: PathBuf,
}
async fn fixture(serve: impl FnOnce(&Releases, &Path) -> (String, Archive, Option<Route>)) -> Fixture {
    let releases = Releases::start().await;
    let directory = tempfile::tempdir().unwrap();
    let marker = std::fs::canonicalize(directory.path()).unwrap().join("agent-ran");
    let (path, archive, route) = serve(&releases, &marker);
    let route = route.unwrap_or_else(|| Route::Body(archive.bytes.clone()));
    let url = releases.serve(&path, route);
    let mut asset = archive.asset;
    asset.url = url;
    let harness = start(
        |base| {
            let empty = base.join("empty-path");
            std::fs::create_dir_all(&empty).unwrap();
            json!({"agy":{"driver":"antigravity","enabled":true,"environment":[{"name":"PATH","value":empty},{"name":"HOME","value":base.join("home")}]}})
        },
        move |_, management| management.configure(|options| options.antigravity_release = Some(Some(asset))),
    )
    .await;
    let (platform, arch) = t3_server::antigravity_install::host_names();
    let managed = harness.base.join("tools/antigravity-acp").join(format!("{platform}-{arch}"));
    Fixture { _marker_directory: directory, harness, releases, managed, marker }
}
async fn installation(fixture: &Fixture) -> AntigravityInstallation {
    fixture.harness.registry.management().antigravity_installation().await.unwrap()
}
/// The states an installation goes through until `wanted`.
async fn states_until(installation: &AntigravityInstallation, wanted: impl Fn(&Value) -> bool) -> Vec<Value> {
    let (current, mut changes) = installation.subscribe();
    let mut seen = vec![current.clone()];
    if wanted(&current) {
        return seen;
    }
    loop {
        let state = within("an installation state", changes.recv()).await.unwrap();
        seen.push(state.as_ref().clone());
        if wanted(&state) {
            return seen;
        }
    }
}
fn leftovers(managed: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(managed.join("versions")).map(|entries| entries.filter_map(|entry| Some(entry.ok()?.file_name().to_string_lossy().into_owned())).collect()).unwrap_or_default();
    names.sort();
    names
}
fn agent_snapshot(fixture: &Fixture) -> Value {
    fixture.harness.registry.snapshots().into_iter().find(|snapshot| snapshot["instanceId"] == "agy").unwrap()
}
async fn install(fixture: &Fixture, manager: &mut Socket) -> Value {
    let started = rpc(manager, 1, "provider.install.start", json!({"instanceId":"agy"})).await;
    success(&started).clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_pinned_runtime_is_downloaded_checked_unpacked_identified_and_only_then_activated() {
    let fixture = fixture(|_, marker| {
        let archive = archive("http://placeholder/".into(), &agent_script(marker, None), |_| {});
        ("/agy.zip".into(), archive, None)
    })
    .await;
    let mut manager = fixture.harness.connect(&fixture.harness.manager).await;
    let before = agent_snapshot(&fixture);
    assert_eq!((before["installed"].clone(), before["status"].clone()), (json!(false), json!("error")));
    assert_eq!(before["setup"], json!({"canAuthenticate":true,"canInstall":true}));
    let started = install(&fixture, &mut manager).await;
    assert_eq!((started["phase"].clone(), started["driver"].clone()), (json!("downloading"), json!("antigravity")));
    let installation = installation(&fixture).await;
    let seen = states_until(&installation, |state| state["phase"] == "succeeded" || state["phase"] == "failed").await;
    let last = seen.last().unwrap();
    assert_eq!((last["phase"].clone(), last["installedVersion"].clone(), last["message"].clone()), (json!("succeeded"), json!(VERSION), Value::Null), "{seen:?}");
    let phases: Vec<&str> = seen.iter().filter_map(|state| state["phase"].as_str()).collect();
    assert!(phases.iter().position(|phase| *phase == "extracting") < phases.iter().position(|phase| *phase == "verifying"), "{phases:?}");
    // On disk: the release by its hash, with its record, and the pointer that activates it.
    let release = fixture.releases.hits.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(release, 1);
    let ids = leftovers(&fixture.managed);
    assert_eq!(ids.len(), 1);
    let directory = fixture.managed.join("versions").join(&ids[0]);
    for (name, mode) in [("agy_acp_server.par", 0o755), ("localharness_external", 0o755)] {
        assert_eq!(std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(directory.join(name)).unwrap().permissions()) & 0o777, mode, "{name}");
    }
    assert_eq!(serde_json::from_str::<Value>(&std::fs::read_to_string(fixture.managed.join("active.json")).unwrap()).unwrap(), json!({"releaseId":ids[0]}));
    assert!(std::fs::read_to_string(&fixture.marker).unwrap().contains("ran"), "the unpacked agent was asked to identify itself");
    // The provider list reads it as installed, at the release's version.
    let after = within("the provider list to read the install", async {
        loop {
            let snapshot = agent_snapshot(&fixture);
            if snapshot["installed"] == true {
                return snapshot;
            }
            let mut changes = fixture.harness.registry.subscribe_changes();
            let _ = changes.recv().await;
        }
    })
    .await;
    assert_eq!((after["version"].clone(), after["status"].clone(), after["auth"]["status"].clone()), (json!(VERSION), json!("warning"), json!("unknown")));
    // Installing again with the same release only checks it.
    let again = install(&fixture, &mut manager).await;
    assert_eq!(again["phase"], "downloading");
    states_until(&installation, |state| state["operationId"] == again["operationId"] && matches!(state["phase"].as_str(), Some("succeeded" | "failed"))).await;
    assert_eq!(fixture.releases.hits.load(std::sync::atomic::Ordering::SeqCst), 1, "an installed release is not downloaded again");
    drop(manager);
    fixture.harness.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_archive_that_fails_any_check_installs_nothing_and_its_agent_never_runs() {
    type Mutate = Box<dyn FnOnce(&mut Vec<(String, Vec<u8>, u32)>) + Send>;
    let cases: Vec<(&str, Mutate, Option<&str>, &str)> = vec![
        ("an extra member", Box::new(|members| members.push(("extra".into(), b"x".to_vec(), FILE_MODE))), None, "The archive must contain exactly the Antigravity executable and its harness."),
        ("a member that leaves the directory", Box::new(|members| members[1].0 = "../localharness_external".into()), None, "The archive contains an unexpected, unsafe, or incorrectly sized member."),
        ("a directory entry", Box::new(|members| members[1].2 = 0o40755 << 16), None, "The archive contains an unexpected, unsafe, or incorrectly sized member."),
        ("a symbolic link", Box::new(|members| members[0].2 = 0o120777 << 16), None, "The archive contains an unexpected, unsafe, or incorrectly sized member."),
        ("a member of the wrong size", Box::new(|members| members[1].1.push(b'x')), Some("size"), "The archive contains an unexpected, unsafe, or incorrectly sized member."),
        ("an agent that does not identify as the release", Box::new(|_| {}), Some("silent"), "The downloaded Antigravity runtime could not start in this environment."),
    ];
    for (what, mutate, flavour, message) in cases {
        let fixture = fixture(move |_, marker| {
            // `size`: the pinned size says one byte fewer than the archive holds.
            let agent = agent_script(marker, if flavour == Some("silent") { Some(1) } else { None });
            let mut archive = archive("http://placeholder/".into(), &agent, mutate);
            if flavour == Some("size") {
                archive.asset.harness.bytes -= 1;
            }
            ("/agy.zip".into(), archive, None)
        })
        .await;
        let mut manager = fixture.harness.connect(&fixture.harness.manager).await;
        install(&fixture, &mut manager).await;
        let installation = installation(&fixture).await;
        let seen = states_until(&installation, |state| matches!(state["phase"].as_str(), Some("succeeded" | "failed"))).await;
        let last = seen.last().unwrap();
        assert_eq!((last["phase"].clone(), last["message"].clone()), (json!("failed"), json!(message)), "{what}");
        assert!(!fixture.managed.join("active.json").exists(), "{what}");
        assert!(leftovers(&fixture.managed).is_empty(), "{what}: nothing published, no staging left: {:?}", leftovers(&fixture.managed));
        if flavour != Some("silent") {
            assert!(!fixture.marker.exists(), "{what}: the agent of a rejected archive never ran");
        }
        drop(manager);
        fixture.harness.finish().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_download_that_is_the_wrong_size_or_hash_is_refused_before_anything_is_unpacked() {
    for (what, route, adjust, message) in [
        ("another file of the right size", 0, 0, "The Antigravity download failed its size or SHA-256 check. Nothing was installed."),
        // A length header that disagrees with the pinned size is refused before a byte is kept.
        ("more than the release is", 1, 0, "The Antigravity download size did not match the pinned release."),
        // A connection that drops has no message of its own: the generic one names no URL or path.
        ("less than it promised", 2, 0, "Could not install Antigravity. Check free disk space and directory access, then try again."),
    ] {
        let _ = adjust;
        let fixture = fixture(move |_, marker| {
            let archive = archive("http://placeholder/".into(), &agent_script(marker, None), |_| {});
            let route = match route {
                0 => {
                    let mut tampered = archive.bytes.clone();
                    tampered[40] ^= 0xff;
                    Route::Body(tampered)
                }
                1 => {
                    let mut longer = archive.bytes.clone();
                    longer.extend([0u8; 16]);
                    Route::Body(longer)
                }
                _ => Route::Truncated { body: archive.bytes[..archive.bytes.len() / 2].to_vec(), declared: archive.bytes.len() },
            };
            ("/agy.zip".into(), archive, Some(route))
        })
        .await;
        let mut manager = fixture.harness.connect(&fixture.harness.manager).await;
        install(&fixture, &mut manager).await;
        let installation = installation(&fixture).await;
        let seen = states_until(&installation, |state| matches!(state["phase"].as_str(), Some("succeeded" | "failed"))).await;
        let last = seen.last().unwrap();
        assert_eq!((last["phase"].clone(), last["message"].clone()), (json!("failed"), json!(message)), "{what}");
        assert!(!seen.iter().any(|state| state["phase"] == "extracting"), "{what}: nothing was unpacked");
        assert!(!fixture.marker.exists() && !fixture.managed.join("active.json").exists(), "{what}");
        assert!(leftovers(&fixture.managed).is_empty(), "{what}");
        drop(manager);
        fixture.harness.finish().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_redirect_to_another_host_is_not_followed() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let followed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = followed.clone();
    let server = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let counted = counted.clone();
            tokio::spawn(async move {
                let mut buffer = [0u8; 2048];
                let read = socket.read(&mut buffer).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                let response = if request.starts_with("GET /start") {
                    format!("HTTP/1.1 302 Found\r\nLocation: http://localhost:{port}/followed\r\nContent-Length: 0\r\n\r\n")
                } else {
                    counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".to_owned()
                };
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let managed = directory.path().join("managed");
    let asset = ReleaseAsset { version: VERSION.into(), url: format!("http://127.0.0.1:{port}/start"), sha256: "0".repeat(64), archive_bytes: 10, executable: File { name: "agy_acp_server.par".into(), bytes: 1 }, harness: File { name: "localharness_external".into(), bytes: 1 } };
    let installation = AntigravityInstallation::open(InstallOptions { managed_directory: managed.clone(), asset: Some(asset), trusted_asset: true, platform: "linux", arch: "x64" }).await;
    installation.start().await.unwrap();
    let seen = states_until(&installation, |state| matches!(state["phase"].as_str(), Some("succeeded" | "failed"))).await;
    assert_eq!(seen.last().unwrap()["phase"], "failed");
    assert_eq!(followed.load(std::sync::atomic::Ordering::SeqCst), 0, "the other host was never asked");
    server.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_download_from_anywhere_but_googles_release_host_needs_code_that_names_it() {
    let directory = tempfile::tempdir().unwrap();
    let asset = ReleaseAsset { version: VERSION.into(), url: "http://127.0.0.1:1/agy.zip".into(), sha256: "0".repeat(64), archive_bytes: 10, executable: File { name: "agy_acp_server.par".into(), bytes: 1 }, harness: File { name: "localharness_external".into(), bytes: 1 } };
    let installation = AntigravityInstallation::open(InstallOptions { managed_directory: directory.path().join("managed"), asset: Some(asset), trusted_asset: false, platform: "linux", arch: "x64" }).await;
    let refused = installation.start().await.unwrap_err();
    assert_eq!(refused.operation, "start");
    assert!(installation.state()["phase"] == "idle", "nothing started");
    // A platform Google publishes nothing for says so.
    let none = AntigravityInstallation::open(InstallOptions { managed_directory: directory.path().join("none"), asset: None, trusted_asset: false, platform: "freebsd", arch: "x64" }).await;
    assert_eq!(none.start().await.unwrap_err().detail, "Google does not publish an Antigravity runtime for freebsd-x64. Use a supported remote environment or a custom executable.");
    // The pinned table only names Google's host.
    for (platform, arch) in [("darwin", "arm64"), ("linux", "x64"), ("win32", "x64")] {
        assert!(t3_server::antigravity_install::release_asset(platform, arch).unwrap().url.starts_with("https://dl.google.com/agy-extensions/releases/"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_a_download_leaves_the_previous_runtime_and_no_staging_and_removal_refuses_paths_in_use() {
    let started = std::sync::Arc::new(tokio::sync::Notify::new());
    let release = std::sync::Arc::new(tokio::sync::Notify::new());
    let (gate_started, gate_release) = (started.clone(), release.clone());
    let fixture = fixture(move |_, marker| {
        let archive = archive("http://placeholder/".into(), &agent_script(marker, None), |_| {});
        let bytes = archive.bytes.clone();
        ("/agy.zip".into(), archive, Some(Route::Gated { body: bytes, first: 40, started: gate_started, release: gate_release }))
    })
    .await;
    let mut manager = fixture.harness.connect(&fixture.harness.manager).await;
    let state = install(&fixture, &mut manager).await;
    within("the download to start", started.notified()).await;
    let cancelled = rpc(&mut manager, 2, "provider.install.cancel", json!({"instanceId":"agy","operationId":state["operationId"]})).await;
    assert_eq!(success(&cancelled)["phase"], "cancelled");
    assert_eq!(success(&cancelled)["message"], "Installation cancelled. The previous runtime is unchanged.");
    release.notify_one();
    assert!(leftovers(&fixture.managed).is_empty() && !fixture.managed.join("active.json").exists());
    assert!(!fixture.marker.exists());
    // Removing what was never installed is a no-op that reports idle.
    let removed = rpc(&mut manager, 3, "provider.install.remove", json!({"instanceId":"agy"})).await;
    assert_eq!(success(&removed)["phase"], "idle");
    drop(manager);
    fixture.harness.finish().await;
}
