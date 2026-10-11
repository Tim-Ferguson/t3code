//! The generated remote shell scripts, run for real in an isolated HOME against a tiny fake release
//! mirror served from `file://` (`archive runner script` in `packages/ssh/src/tunnel.test.ts`), and
//! the manager driving them through a fake `ssh` executable that runs the "remote" command locally.
//! Nothing here reaches a network or the user's HOME; the fake `t3` is a shell script, and the one
//! background process the launch script starts is stopped by the stop script.
#![cfg(unix)]
mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use support::ssh_fakes::*;
use support::{temp_dir, within, write_script};
use t3_remote::ssh::{SshTarget, remote_state_key};
use t3_remote::ssh_scripts::*;
use t3_remote::ssh_tunnel::*;

const VERSION: &str = "1.2.3-preview.20260911.4";
const SYSTEM_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// Packs a fake release the way the release workflow does: one top-level directory named after the
/// stem, checksummed in SHA256SUMS. `t3` answers what the launch script asks of it.
fn make_mirror(root: &Path) -> String {
    let platform = if cfg!(target_os = "macos") { "darwin" } else { "linux" };
    let arch = if cfg!(target_arch = "aarch64") { "arm64" } else { "x64" };
    let stem = format!("t3-{VERSION}-{platform}-{arch}");
    let stage = root.join("stage").join(&stem);
    let release = root.join("mirror").join(format!("v{VERSION}"));
    std::fs::create_dir_all(&stage).unwrap();
    std::fs::create_dir_all(&release).unwrap();
    let t3 = stage.join("t3");
    std::fs::write(
        &t3,
        format!(
            r#"#!/bin/sh
case "$1" in
  --version) echo "t3 v{VERSION}" ;;
  __ssh-helper)
    case "$2" in
      pick-port) printf 43999 ;;
      wait-ready) exit 0 ;;
      *) exit 1 ;;
    esac ;;
  serve) exec sleep 120 ;;
  auth) echo '{{"credential":"E2E-CREDENTIAL"}}' ;;
esac
"#
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&t3, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let script = format!(
        "set -eu\ntar -czf '{release}/{stem}.tar.gz' -C '{stage_root}' '{stem}'\ncd '{release}' && (sha256sum '{stem}.tar.gz' 2>/dev/null || shasum -a 256 '{stem}.tar.gz') > SHA256SUMS",
        release = release.display(),
        stage_root = root.join("stage").display(),
    );
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(script)
        .env_clear()
        .env("PATH", SYSTEM_PATH)
        .status()
        .unwrap();
    assert!(status.success());
    format!("file://{}", root.join("mirror").display())
}

/// A `sleep` that sleeps for a twentieth of a second whatever it is asked, so the runner's
/// one-second lock polling does not slow the test.
fn sleep_shim(dir: &Path) -> PathBuf {
    let bin = dir.join("shim-bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(&bin, "sleep", "exec /bin/sleep 0.05");
    bin
}

async fn run_runner(home: &Path, runner: &Path, shim: &Path) -> (i32, String, String) {
    let output = tokio::process::Command::new("sh")
        .arg(runner)
        .arg("--version")
        .env_clear()
        .env("PATH", format!("{}:{SYSTEM_PATH}", shim.display()))
        .env("HOME", home)
        .output()
        .await
        .unwrap();
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// String assertions cannot prove the lock excludes concurrent installers, so the real script runs.
#[tokio::test]
async fn the_archive_runner_installs_once_when_launches_race_and_reclaims_stale_locks() {
    let root = temp_dir("runner-script");
    let release_base_url = make_mirror(root.path());
    let shim = sleep_shim(root.path());
    let runner = root.path().join("run-t3.sh");
    std::fs::write(
        &runner,
        build_remote_t3_runner_script(Some(&RemoteT3RunnerOptions {
            archive_version: Some(VERSION.into()),
            release_base_url: Some(release_base_url),
            ..Default::default()
        }))
        .unwrap(),
    )
    .unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    let (a, b, c) = within(async {
        tokio::join!(
            run_runner(&home, &runner, &shim),
            run_runner(&home, &runner, &shim),
            run_runner(&home, &runner, &shim)
        )
    })
    .await;
    for (code, stdout, stderr) in [a, b, c] {
        assert_eq!(code, 0, "{stderr}");
        assert!(stdout.contains(&format!("t3 v{VERSION}")), "{stdout}");
    }
    let versions = home.join(".t3/runtime/versions");
    let entries: Vec<String> = std::fs::read_dir(&versions)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries, [VERSION]);
    assert_eq!(
        std::fs::read_to_string(versions.join(VERSION).join(".install-complete")).unwrap().trim(),
        VERSION
    );

    // A lock left by a crashed installer (dead pid) must not block the next launch, and neither
    // must one that never published a pid.
    let lock = versions.join(format!(".{VERSION}.install.lock"));
    std::fs::remove_dir_all(versions.join(VERSION)).unwrap();
    std::fs::create_dir(&lock).unwrap();
    std::fs::write(lock.join("pid"), "999999\n").unwrap();
    let (code, _, stderr) = within(run_runner(&home, &runner, &shim)).await;
    assert_eq!(code, 0, "{stderr}");

    std::fs::remove_dir_all(versions.join(VERSION)).unwrap();
    std::fs::create_dir(&lock).unwrap();
    let (code, _, stderr) = within(run_runner(&home, &runner, &shim)).await;
    assert_eq!(code, 0, "{stderr}");
    assert!(!lock.exists());
}

#[tokio::test]
async fn a_checksum_mismatch_is_refused_and_installs_nothing() {
    let root = temp_dir("runner-checksum");
    let release_base_url = make_mirror(root.path());
    let sums = root.path().join("mirror").join(format!("v{VERSION}")).join("SHA256SUMS");
    let text = std::fs::read_to_string(&sums).unwrap();
    std::fs::write(&sums, text.replacen(&text[..8], "00000000", 1)).unwrap();
    let runner = root.path().join("run-t3.sh");
    std::fs::write(
        &runner,
        build_remote_t3_runner_script(Some(&RemoteT3RunnerOptions {
            archive_version: Some(VERSION.into()),
            release_base_url: Some(release_base_url),
            ..Default::default()
        }))
        .unwrap(),
    )
    .unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let (code, _, stderr) = within(run_runner(&home, &runner, &sleep_shim(root.path()))).await;
    assert_eq!(code, 1);
    assert!(stderr.contains("Checksum mismatch"), "{stderr}");
    assert!(!home.join(".t3/runtime/versions").join(VERSION).exists());
}

/// A `ssh` that answers `-G`, stays up as a tunnel for `-N`, and otherwise runs the remote command
/// (`sh [-l] -s ...`) locally with an isolated HOME.
fn local_ssh(dir: &Path, home: &Path) -> PathBuf {
    write_script(
        dir,
        "ssh",
        &format!(
            r#"while [ $# -gt 0 ]; do
  case "$1" in
    -o|-p) shift 2 ;;
    -G) echo "hostname devbox.example.com"; echo "user julius"; echo "port 2222"; exit 0 ;;
    -n) shift ;;
    -N) exec sleep 600 ;;
    *) break ;;
  esac
done
shift
[ "$1" = sh ] && shift
[ "$1" = "-l" ] && shift
exec env HOME='{home}' PATH='{SYSTEM_PATH}' sh "$@""#,
            home = home.display()
        ),
    )
}

fn alive(pid: &str) -> bool {
    // SAFETY: signal 0 only tests whether the process the launch script recorded still exists.
    unsafe { libc::kill(pid.trim().parse().unwrap(), 0) == 0 }
}

/// The whole path against the real launch, pairing and stop scripts: a managed server is started
/// from the installed archive, the tunnel is forwarded, a pairing credential is minted, the second
/// connect reuses everything, and disconnect stops the server and clears the ownership files.
#[tokio::test]
async fn connect_pair_reuse_and_disconnect_run_the_real_scripts() {
    let root = temp_dir("remote-e2e");
    let release_base_url = make_mirror(root.path());
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let ssh = local_ssh(root.path(), &home);
    let rt = runtime(
        Arc::new(ProcessSshSpawner::new(ssh.display().to_string())),
        FakeProbe::ok(),
        root.path(),
    );
    let manager = SshEnvironmentManager::new(
        rt,
        SshEnvironmentManagerOptions {
            resolve_cli_runner: Some(Box::new(move || RemoteT3RunnerOptions {
                archive_version: Some(VERSION.into()),
                release_base_url: Some(release_base_url.clone()),
                ..Default::default()
            })),
        },
    );
    let requested = SshTarget {
        alias: "devbox".into(),
        hostname: "devbox".into(),
        username: None,
        port: None,
    };
    let resolved = SshTarget {
        alias: "devbox".into(),
        hostname: "devbox.example.com".into(),
        username: Some("julius".into()),
        port: Some(2222),
    };
    let state_dir = home.join(".t3/ssh-launch").join(remote_state_key(&resolved));

    let bootstrap = within(manager.ensure_environment(&requested, true)).await.unwrap();
    assert_eq!(bootstrap.remote_port, 43_999);
    assert_eq!(bootstrap.remote_server_kind, Some(RemoteServerKind::Managed));
    assert_eq!(bootstrap.pairing_token.as_deref(), Some("E2E-CREDENTIAL"));
    assert_eq!(bootstrap.target, resolved);
    let pid = std::fs::read_to_string(state_dir.join("pid")).unwrap();
    assert_eq!(std::fs::read_to_string(state_dir.join("port")).unwrap().trim(), "43999");
    assert_eq!(std::fs::read_to_string(state_dir.join("managed")).unwrap().trim(), "managed");
    assert!(alive(&pid), "the managed server runs");
    assert!(home.join(".t3/runtime/versions").join(VERSION).join(".install-complete").exists());

    let again = within(manager.ensure_environment(&requested, false)).await.unwrap();
    assert_eq!(again.http_base_url, bootstrap.http_base_url);

    within(manager.disconnect_environment(&requested)).await.unwrap();
    assert!(!alive(&pid), "the stop script stopped the server");
    for file in ["pid", "port", "managed"] {
        assert!(!state_dir.join(file).exists(), "{file}");
    }
    manager.shutdown().await;
}
