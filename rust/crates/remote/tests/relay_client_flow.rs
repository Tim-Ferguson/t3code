//! Finding, installing and running the relay client (`cloudflared`) with fake executables and a
//! release server on loopback.
#![cfg(unix)]
mod support;

use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};
use support::*;
use t3_remote::endpoint_runtime::*;
use t3_remote::relay_client::*;

fn host() -> HostPlatform {
    HostPlatform {
        platform: "linux".into(),
        arch: "x64".into(),
    }
}

fn client(
    base: &std::path::Path,
    override_path: Option<&str>,
    path_env: Option<&str>,
    asset: Option<ReleaseAsset>,
) -> Cloudflared {
    Cloudflared::new(CloudflaredOptions {
        base_dir: base.to_path_buf(),
        host: host(),
        executable_override: override_path.map(str::to_owned),
        path_env: path_env.map(str::to_owned),
        release_asset: asset,
    })
}

#[test]
fn the_pinned_release_assets_are_the_originals() {
    let linux = release_asset(&host()).unwrap();
    assert_eq!(
        linux.url,
        "https://github.com/cloudflare/cloudflared/releases/download/2026.5.2/cloudflared-linux-amd64"
    );
    assert_eq!(
        linux.sha256,
        "5286698547f03df745adb2355f04c12dde52ef425491e81f433642d695521886"
    );
    assert_eq!(linux.archive, ArchiveKind::Binary);
    let mac = release_asset(&HostPlatform {
        platform: "darwin".into(),
        arch: "arm64".into(),
    })
    .unwrap();
    assert!(mac.url.ends_with("cloudflared-darwin-arm64.tgz") && mac.archive == ArchiveKind::Tgz);
    assert_eq!(
        mac.sha256,
        "ba94054c9fd4297645093d59d51442e5e546d07bb0516120e694a13d5b216d38"
    );
    let win = release_asset(&HostPlatform {
        platform: "win32".into(),
        arch: "x64".into(),
    })
    .unwrap();
    assert!(win.url.ends_with("cloudflared-windows-amd64.exe"));
    assert!(
        release_asset(&HostPlatform {
            platform: "freebsd".into(),
            arch: "x64".into()
        })
        .is_none()
    );
    assert_eq!(CLOUDFLARED_VERSION, "2026.5.2");
}

#[tokio::test]
async fn resolution_prefers_the_override_then_the_managed_copy_then_path() {
    let dir = temp_dir("relay-client-resolve");
    let base = dir.path().join("home");
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let on_path = write_script(&bin, "cloudflared", "exit 0");
    let path_env = format!("/nonexistent:{}:", bin.display());

    // Nothing anywhere: missing (there is a release for this platform).
    let c = client(&base, None, Some("/nonexistent"), None);
    assert_eq!(
        c.resolve().await,
        RelayClientStatus::Missing {
            version: CLOUDFLARED_VERSION.into()
        }
    );
    // On PATH.
    let c = client(&base, None, Some(&path_env), None);
    assert_eq!(
        c.resolve().await,
        RelayClientStatus::Available {
            executable_path: on_path.display().to_string(),
            source: ExecutableSource::Path,
            version: CLOUDFLARED_VERSION.into()
        }
    );
    // The managed copy beats PATH.
    let managed = c.managed_path().to_path_buf();
    assert!(managed.ends_with(format!(
        "tools/cloudflared/{CLOUDFLARED_VERSION}/linux-x64/cloudflared"
    )));
    std::fs::create_dir_all(managed.parent().unwrap()).unwrap();
    write_script(managed.parent().unwrap(), "cloudflared", "exit 0");
    assert!(matches!(
        c.resolve().await,
        RelayClientStatus::Available {
            source: ExecutableSource::Managed,
            ..
        }
    ));
    // An override beats both, and a bad override is missing rather than falling through.
    let custom = write_script(dir.path(), "custom-cf", "exit 0");
    let c = client(&base, Some(custom.to_str().unwrap()), Some(&path_env), None);
    assert!(matches!(
        c.resolve().await,
        RelayClientStatus::Available {
            source: ExecutableSource::Override,
            ..
        }
    ));
    let c = client(&base, Some("/nonexistent/cf"), Some(&path_env), None);
    assert_eq!(
        c.resolve().await,
        RelayClientStatus::Missing {
            version: CLOUDFLARED_VERSION.into()
        }
    );
    // A blank override counts as unset.
    let c = client(
        &dir.path().join("other-home"),
        Some("  "),
        Some(&path_env),
        None,
    );
    assert!(matches!(
        c.resolve().await,
        RelayClientStatus::Available {
            source: ExecutableSource::Path,
            ..
        }
    ));
    // A file on PATH that is not executable is not found.
    let plain = dir.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    std::fs::write(plain.join("cloudflared"), "x").unwrap();
    let c = client(
        &dir.path().join("other-home"),
        None,
        Some(plain.to_str().unwrap()),
        None,
    );
    assert!(matches!(
        c.resolve().await,
        RelayClientStatus::Missing { .. }
    ));
    // No release for the platform: unsupported.
    let odd = Cloudflared::new(CloudflaredOptions {
        base_dir: base,
        host: HostPlatform {
            platform: "freebsd".into(),
            arch: "x64".into(),
        },
        executable_override: None,
        path_env: None,
        release_asset: None,
    });
    assert_eq!(
        odd.resolve().await,
        RelayClientStatus::Unsupported {
            platform: "freebsd".into(),
            arch: "x64".into(),
            version: CLOUDFLARED_VERSION.into()
        }
    );
}

#[test]
fn status_and_progress_serialise_as_the_rpc_contracts_do() {
    let available = RelayClientStatus::Available {
        executable_path: "/x".into(),
        source: ExecutableSource::Managed,
        version: "v".into(),
    };
    assert_eq!(
        serde_json::to_value(&available).unwrap(),
        serde_json::json!({"status": "available", "executablePath": "/x", "source": "managed", "version": "v"})
    );
    assert_eq!(
        serde_json::to_value(RelayClientStatus::Missing {
            version: "v".into()
        })
        .unwrap(),
        serde_json::json!({"status": "missing", "version": "v"})
    );
    assert_eq!(
        serde_json::to_value(RelayClientStatus::Unsupported {
            platform: "p".into(),
            arch: "a".into(),
            version: "v".into()
        })
        .unwrap(),
        serde_json::json!({"status": "unsupported", "platform": "p", "arch": "a", "version": "v"})
    );
    assert_eq!(
        serde_json::to_value(InstallProgressEvent::Progress {
            stage: InstallProgressStage::WaitingForLock
        })
        .unwrap(),
        serde_json::json!({"type": "progress", "stage": "waiting_for_lock"})
    );
    assert_eq!(
        serde_json::to_value(InstallProgressEvent::Complete { status: available }).unwrap()["type"],
        "complete"
    );
}

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

async fn release_server(body: &'static [u8]) -> FakeRelay {
    FakeRelay::start_bytes(body).await
}

#[tokio::test]
async fn installing_downloads_verifies_validates_and_activates_the_pinned_binary() {
    let dir = temp_dir("relay-client-install");
    let script: &'static [u8] = b"#!/bin/sh\nif [ \"$1\" = version ]; then echo 'cloudflared version test'; exit 0; fi\nexit 0\n";
    let server = release_server(script).await;
    let asset = ReleaseAsset {
        url: format!("{}/cloudflared-linux-amd64", server.url),
        sha256: sha(script),
        archive: ArchiveKind::Binary,
    };
    let c = client(dir.path(), None, Some("/nonexistent"), Some(asset));
    let stages = Arc::new(Mutex::new(vec![]));
    let seen = stages.clone();
    let status = within(c.install_with_progress(&move |stage| seen.lock().unwrap().push(stage)))
        .await
        .unwrap();
    assert_eq!(
        *stages.lock().unwrap(),
        [
            InstallProgressStage::Checking,
            InstallProgressStage::WaitingForLock,
            InstallProgressStage::Downloading,
            InstallProgressStage::Verifying,
            InstallProgressStage::Installing,
            InstallProgressStage::Validating,
            InstallProgressStage::Activating,
        ]
    );
    let RelayClientStatus::Available {
        executable_path,
        source,
        ..
    } = &status
    else {
        panic!("{status:?}")
    };
    assert_eq!(*source, ExecutableSource::Managed);
    assert_eq!(std::path::Path::new(executable_path), c.managed_path());
    assert_eq!(c.resolve().await, status);
    // No lock or staging files are left behind.
    let leftovers: Vec<_> = std::fs::read_dir(c.managed_path().parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(leftovers, ["cloudflared"]);
    // Installing again is a no-op that only checks.
    let stages = Arc::new(Mutex::new(vec![]));
    let seen = stages.clone();
    within(c.install_with_progress(&move |stage| seen.lock().unwrap().push(stage)))
        .await
        .unwrap();
    assert_eq!(*stages.lock().unwrap(), [InstallProgressStage::Checking]);
    assert_eq!(server.recorded().len(), 1);
}

#[tokio::test]
async fn a_download_that_does_not_match_the_pinned_checksum_is_refused() {
    let dir = temp_dir("relay-client-checksum");
    let server = release_server(b"#!/bin/sh\nexit 0\n").await;
    let asset = ReleaseAsset {
        url: format!("{}/cf", server.url),
        sha256: sha(b"something else"),
        archive: ArchiveKind::Binary,
    };
    let c = client(dir.path(), None, Some("/nonexistent"), Some(asset));
    let error = within(c.install_with_progress(&|_| {})).await.unwrap_err();
    assert_eq!(error.reason, InstallFailureReason::InvalidChecksum);
    assert_eq!(
        error.message,
        "Downloaded relay client checksum did not match the pinned release."
    );
    assert!(!c.managed_path().exists());
}

#[tokio::test]
async fn a_binary_that_does_not_run_is_not_activated() {
    let dir = temp_dir("relay-client-validate");
    let script: &'static [u8] = b"#!/bin/sh\nexit 3\n";
    let server = release_server(script).await;
    let asset = ReleaseAsset {
        url: format!("{}/cf", server.url),
        sha256: sha(script),
        archive: ArchiveKind::Binary,
    };
    let c = client(dir.path(), None, Some("/nonexistent"), Some(asset));
    let error = within(c.install_with_progress(&|_| {})).await.unwrap_err();
    assert_eq!(error.reason, InstallFailureReason::ValidationFailed);
    assert!(!c.managed_path().exists());
}

#[tokio::test]
async fn install_failures_name_their_reason() {
    let dir = temp_dir("relay-client-reasons");
    // A download that fails.
    let down = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let asset = ReleaseAsset {
        url: format!("http://127.0.0.1:{down}/cf"),
        sha256: sha(b""),
        archive: ArchiveKind::Binary,
    };
    let error = within(
        client(dir.path(), None, Some("/nonexistent"), Some(asset)).install_with_progress(&|_| {}),
    )
    .await
    .unwrap_err();
    assert_eq!(error.reason, InstallFailureReason::DownloadFailed);
    // An override that is not an executable.
    let error = within(
        client(dir.path(), Some("/nonexistent/cf"), None, None).install_with_progress(&|_| {}),
    )
    .await
    .unwrap_err();
    assert_eq!(error.reason, InstallFailureReason::OverrideMissing);
    assert_eq!(
        error.message,
        "T3CODE_CLOUDFLARED_PATH does not point to an executable file."
    );
    // A platform with no managed binary.
    let odd = Cloudflared::new(CloudflaredOptions {
        base_dir: dir.path().to_path_buf(),
        host: HostPlatform {
            platform: "freebsd".into(),
            arch: "x64".into(),
        },
        executable_override: None,
        path_env: None,
        release_asset: None,
    });
    let error = within(odd.install_with_progress(&|_| {}))
        .await
        .unwrap_err();
    assert_eq!(error.reason, InstallFailureReason::UnsupportedPlatform);
    assert_eq!(
        error.message,
        "T3 Code does not provide a managed relay client binary for freebsd-x64."
    );
}

#[tokio::test]
async fn a_stale_install_lock_is_replaced() {
    let dir = temp_dir("relay-client-lock");
    let script: &'static [u8] = b"#!/bin/sh\nexit 0\n";
    let server = release_server(script).await;
    let asset = ReleaseAsset {
        url: format!("{}/cf", server.url),
        sha256: sha(script),
        archive: ArchiveKind::Binary,
    };
    let c = client(dir.path(), None, Some("/nonexistent"), Some(asset));
    let lock = std::path::PathBuf::from(format!("{}.lock", c.managed_path().display()));
    std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
    std::fs::write(&lock, "").unwrap();
    // Make the lock look ten minutes old.
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(600);
    std::fs::File::options()
        .write(true)
        .open(&lock)
        .unwrap()
        .set_modified(old)
        .unwrap();
    within(c.install_with_progress(&|_| {})).await.unwrap();
    assert!(c.managed_path().exists());
    assert!(!lock.exists());
}

// ---- running a connector -------------------------------------------------------------------------------

#[tokio::test]
async fn the_real_spawner_streams_output_lines_and_reports_the_exit() {
    let dir = temp_dir("connector-spawner");
    let script = write_script(
        dir.path(),
        "cloudflared",
        r#"echo "args: $*"; echo "token: $TUNNEL_TOKEN"; echo "INF Registered tunnel connection connIndex=0" >&2; exit 7"#,
    );
    let spawner = ProcessConnectorSpawner;
    let mut connector = spawner
        .spawn(script.to_str().unwrap(), "secret-token")
        .await
        .unwrap();
    assert!(connector.pid > 0);
    let mut lines = vec![];
    while let Some(line) = within(connector.output.recv()).await {
        lines.push(line);
    }
    lines.sort();
    assert_eq!(
        lines,
        [
            "INF Registered tunnel connection connIndex=0",
            "args: tunnel --no-autoupdate --loglevel info --output default run",
            "token: secret-token",
        ]
    );
    assert_eq!(within(connector.exit).await.unwrap(), Some(7));
    assert!(!connector.control.is_running());
    assert!(
        spawner
            .spawn("/nonexistent/cloudflared", "t")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn the_real_spawner_can_stop_a_running_connector() {
    let dir = temp_dir("connector-kill");
    let script = write_script(dir.path(), "cloudflared", "echo started; exec sleep 600");
    let mut connector = ProcessConnectorSpawner
        .spawn(script.to_str().unwrap(), "t")
        .await
        .unwrap();
    assert_eq!(
        within(connector.output.recv()).await.as_deref(),
        Some("started")
    );
    assert!(connector.control.is_running());
    connector.control.kill();
    let code = within(connector.exit).await.unwrap();
    assert_ne!(code, Some(0));
    assert!(!connector.control.is_running());
}

#[tokio::test]
async fn the_endpoint_runtime_runs_a_real_process_end_to_end() {
    let dir = temp_dir("connector-runtime");
    let script = write_script(
        dir.path(),
        "cloudflared",
        r#"echo "INF Registered tunnel connection connIndex=0"; exec sleep 600"#,
    );
    let resolver = Arc::new(FixedResolver(Mutex::new(RelayClientStatus::Available {
        executable_path: script.display().to_string(),
        source: ExecutableSource::Override,
        version: "test".into(),
    })));
    let runtime = ManagedEndpointRuntime::new(resolver, Arc::new(ProcessConnectorSpawner));
    let status = runtime
        .apply_config(Some(&t3_remote::types::RuntimeConfig::cloudflare(
            "tok",
            Some("t"),
        )))
        .await;
    assert!(status.is_running());
    within(runtime.next_tunnel_connection()).await;
    assert_eq!(runtime.apply_config(None).await, EndpointStatus::Disabled);
}
