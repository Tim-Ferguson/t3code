//! The askpass helper, the `__ssh-helper` subcommands and the `pair --tailscale` decisions
//! (`packages/ssh/src/auth.test.ts`, `apps/server/src/cli/pair.test.ts`,
//! `apps/server/src/cli/sshHelper.ts`). Loopback sockets and temp directories only; no `tailscale`
//! or `ssh` runs.
#![cfg(unix)]
mod support;

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use support::{FakeRelay, temp_dir, within, write_script};
use t3_remote::ssh_auth::*;
use t3_remote::ssh_helper;
use t3_remote::tailscale::TailscaleStatus;
use t3_remote::tailscale_pair::*;

fn host(temp_root: &std::path::Path) -> SshAuthHost {
    SshAuthHost {
        windows: false,
        process_display: None,
        temp_root: temp_root.to_path_buf(),
    }
}

// ---- askpass ---------------------------------------------------------------------------------------

#[test]
fn ssh_auth_failures_are_told_from_other_errors() {
    assert!(is_ssh_auth_failure(
        "julius@100.65.180.100: Permission denied (publickey,password,keyboard-interactive)."
    ));
    assert!(is_ssh_auth_failure("Permission denied (publickey)."));
    assert!(!is_ssh_auth_failure("Connection timed out"));
    assert!(!is_ssh_auth_failure("mkdir: Permission denied"));
}

#[test]
fn askpass_environment_serves_the_cached_password() {
    let root = temp_dir("askpass");
    let directory = root.path().join("askpass");
    let env = build_ssh_child_environment(
        &SshChildEnvironmentOptions {
            interactive_auth: true,
            base_env: Some(BTreeMap::new()),
            askpass_directory: Some(directory.display().to_string()),
            auth_secret: Some(Some("super-secret".into())),
        },
        &host(root.path()),
    )
    .unwrap();
    let askpass = directory.join("ssh-askpass.sh");
    assert_eq!(env["SSH_ASKPASS"], askpass.display().to_string());
    assert_eq!(env["SSH_ASKPASS_REQUIRE"], "force");
    assert_eq!(env["T3_SSH_AUTH_SECRET"], "super-secret");
    assert_eq!(env["DISPLAY"], "t3code");
    let contents = std::fs::read_to_string(&askpass).unwrap();
    assert!(contents.contains("printf \"%s\\n\" \"$T3_SSH_AUTH_SECRET\""));
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&askpass).unwrap().permissions().mode() & 0o777, 0o700);
    }

    // The helper prints the secret when it is set (even empty) and fails loudly otherwise.
    let run = |secret: Option<&str>| {
        let mut command = std::process::Command::new("sh");
        command.arg(&askpass).env_clear();
        if let Some(secret) = secret {
            command.env("T3_SSH_AUTH_SECRET", secret);
        }
        command.output().unwrap()
    };
    let served = run(Some("p w"));
    assert!(served.status.success());
    assert_eq!(String::from_utf8_lossy(&served.stdout), "p w\n");
    assert_eq!(String::from_utf8_lossy(&run(Some("")).stdout), "\n");
    let missing = run(None);
    assert_eq!(missing.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&missing.stderr).contains("without T3_SSH_AUTH_SECRET"));
}

#[test]
fn child_environment_rules() {
    let root = temp_dir("askpass-env");
    // Not interactive: the base environment unchanged.
    let base = BTreeMap::from([("A".to_owned(), "1".to_owned())]);
    let plain = build_ssh_child_environment(
        &SshChildEnvironmentOptions {
            base_env: Some(base.clone()),
            ..Default::default()
        },
        &host(root.path()),
    )
    .unwrap();
    assert_eq!(plain, base);

    let options = |base_env: Option<BTreeMap<String, String>>, secret: Option<Option<String>>| {
        SshChildEnvironmentOptions {
            interactive_auth: true,
            base_env,
            askpass_directory: Some(root.path().join("d").display().to_string()),
            auth_secret: secret,
        }
    };
    // No secret option: no variable; a null secret: an empty one.
    let none = build_ssh_child_environment(&options(Some(BTreeMap::new()), None), &host(root.path())).unwrap();
    assert!(!none.contains_key("T3_SSH_AUTH_SECRET"));
    let null = build_ssh_child_environment(&options(Some(BTreeMap::new()), Some(None)), &host(root.path())).unwrap();
    assert_eq!(null["T3_SSH_AUTH_SECRET"], "");
    // An existing display (base or, without a base, the process) is left alone.
    let displayed = BTreeMap::from([("DISPLAY".to_owned(), ":0".to_owned())]);
    let kept = build_ssh_child_environment(&options(Some(displayed), None), &host(root.path())).unwrap();
    assert_eq!(kept["DISPLAY"], ":0");
    let mut with_display = host(root.path());
    with_display.process_display = Some(":1".into());
    let from_process = build_ssh_child_environment(&options(None, None), &with_display).unwrap();
    assert!(!from_process.contains_key("DISPLAY"));
    // The default askpass directory is a fresh private one under the temp root.
    let default_dir = build_ssh_child_environment(
        &SshChildEnvironmentOptions {
            interactive_auth: true,
            ..Default::default()
        },
        &host(root.path()),
    )
    .unwrap();
    assert!(default_dir["SSH_ASKPASS"].contains("t3code-ssh-runtime-"));
    assert!(default_dir["SSH_ASKPASS"].starts_with(&root.path().display().to_string()));
}

#[test]
fn windows_gets_a_launcher_pair() {
    let descriptor = build_ssh_askpass_helper_descriptor("C:\\temp\\t3code-ssh-askpass", true);
    assert_eq!(descriptor.launcher_path, "C:\\temp\\t3code-ssh-askpass\\ssh-askpass.cmd");
    let names: Vec<&str> = descriptor
        .files
        .iter()
        .map(|f| f.path.rsplit('\\').next().unwrap())
        .collect();
    assert_eq!(names, ["ssh-askpass.cmd", "ssh-askpass.ps1"]);
}

// ---- __ssh-helper ---------------------------------------------------------------------------------------

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

#[test]
fn pick_port_starts_at_the_recorded_port_and_skips_taken_ones() {
    let dir = temp_dir("pick-port");
    let file = dir.path().join("port");
    // Hold a loopback port, record it as preferred: the next one is picked.
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let taken = held.local_addr().unwrap().port();
    std::fs::write(&file, format!("{taken}\n")).unwrap();
    let picked = ssh_helper::pick_port(&file, 3773, 50).unwrap();
    assert!(picked > i64::from(taken) && picked < i64::from(taken) + 50);
    // No file: the default is scanned from; a window of 0 finds nothing.
    let missing = dir.path().join("missing");
    assert!(ssh_helper::pick_port(&missing, i64::from(free_port()), 20).is_some());
    assert_eq!(ssh_helper::pick_port(&missing, 3773, 0), None);
    // Garbage in the file falls back to the default; ports past 65535 fail the scan.
    std::fs::write(&file, "not a port").unwrap();
    assert!(ssh_helper::pick_port(&file, i64::from(free_port()), 20).is_some());
    assert_eq!(ssh_helper::pick_port(&missing, 65_536, 5), None);
}

#[tokio::test]
async fn wait_ready_succeeds_as_soon_as_the_server_answers_2xx() {
    let server = FakeRelay::start(|_| (200, json!({}))).await;
    let port: i64 = server.url.rsplit(':').next().unwrap().parse().unwrap();
    assert!(within(ssh_helper::wait_ready(port, Duration::from_secs(30), Duration::from_secs(5))).await);
}

/// The deadline is virtual time here (a paused clock also expires the probe bound while a request
/// is in flight, which is fine: neither server ever answers 2xx).
#[tokio::test(start_paused = true)]
async fn wait_ready_gives_up_at_the_deadline() {
    let failing = FakeRelay::start(|_| (503, json!({}))).await;
    let port: i64 = failing.url.rsplit(':').next().unwrap().parse().unwrap();
    assert!(!within(ssh_helper::wait_ready(port, Duration::from_millis(500), Duration::from_secs(1))).await);
    let closed = i64::from(free_port());
    assert!(!within(ssh_helper::wait_ready(closed, Duration::from_millis(300), Duration::from_secs(1))).await);
}

#[test]
fn runtime_port_reports_a_live_loopback_server_only() {
    let dir = temp_dir("runtime-port");
    let file = dir.path().join("server-runtime.json");
    let me = i64::from(std::process::id());
    let write = |value: serde_json::Value| std::fs::write(&file, value.to_string()).unwrap();
    write(json!({"pid": me, "port": 3773, "origin": "http://127.0.0.1:3773"}));
    assert_eq!(ssh_helper::runtime_port(&file), Some((me, 3773)));
    write(json!({"pid": me.to_string(), "port": "3774", "origin": "http://localhost:3774"}));
    assert_eq!(ssh_helper::runtime_port(&file), Some((me, 3774)));
    for bad in [
        json!({"pid": me, "port": 3773, "origin": "https://127.0.0.1:3773"}),
        json!({"pid": me, "port": 3773, "origin": "http://192.168.1.2:3773"}),
        json!({"pid": me, "port": 3773}),
        json!({"pid": me, "port": 3773.5, "origin": "http://127.0.0.1:1"}),
        json!({"pid": 0, "port": 3773, "origin": "http://127.0.0.1:1"}),
        json!({"pid": -5, "port": 3773, "origin": "http://127.0.0.1:1"}),
        json!({"port": 3773, "origin": "http://127.0.0.1:1"}),
        json!({"pid": 2_147_483_647_i64, "port": 3773, "origin": "http://127.0.0.1:1"}),
    ] {
        write(bad.clone());
        assert_eq!(ssh_helper::runtime_port(&file), None, "{bad}");
    }
    std::fs::write(&file, "{not json").unwrap();
    assert_eq!(ssh_helper::runtime_port(&file), None);
    assert_eq!(ssh_helper::runtime_port(&dir.path().join("nothing")), None);
}

#[tokio::test]
async fn the_subcommands_print_what_the_launch_script_parses() {
    let dir = temp_dir("helper-run");
    let file = dir.path().join("port");
    let port = free_port();
    let out = ssh_helper::run(&[
        "pick-port".into(),
        file.display().to_string(),
        port.to_string(),
        "10".into(),
    ])
    .await;
    assert_eq!((out.exit_code, out.stdout.parse::<u16>().is_ok()), (0, true));
    let out = ssh_helper::run(&["runtime-port".into(), dir.path().join("none").display().to_string()]).await;
    assert_eq!((out.exit_code, out.stdout.as_str()), (1, ""));
    let out = ssh_helper::run(&["nonsense".into()]).await;
    assert_eq!(out.exit_code, 2);
}

// ---- pair --tailscale ----------------------------------------------------------------------------------------

struct FakeTailscale {
    status: Result<TailscaleStatus, String>,
    serve: Result<(), String>,
    serves: Mutex<Vec<(u16, u16, Option<String>)>>,
}

impl FakeTailscale {
    fn named(name: Option<&str>) -> Self {
        Self {
            status: Ok(TailscaleStatus {
                magic_dns_name: name.map(str::to_owned),
                tailnet_ipv4_addresses: vec![],
            }),
            serve: Ok(()),
            serves: Mutex::new(vec![]),
        }
    }
}

#[async_trait]
impl PairTailscale for FakeTailscale {
    async fn read_status(&self) -> Result<TailscaleStatus, String> {
        self.status.clone()
    }

    async fn ensure_serve(&self, local_port: u16, serve_port: u16, local_host: Option<&str>) -> Result<(), String> {
        self.serves.lock().unwrap().push((local_port, serve_port, local_host.map(str::to_owned)));
        self.serve.clone()
    }
}

/// Probe results in order (the last repeats), and the URLs asked.
struct ScriptedProber {
    results: Mutex<Vec<EnvironmentProbeResult>>,
    urls: Mutex<Vec<String>>,
}

impl ScriptedProber {
    fn new(results: Vec<EnvironmentProbeResult>) -> Self {
        Self {
            results: Mutex::new(results),
            urls: Mutex::new(vec![]),
        }
    }
}

#[async_trait]
impl EnvironmentProber for ScriptedProber {
    async fn probe(&self, base_url: &str) -> EnvironmentProbeResult {
        self.urls.lock().unwrap().push(base_url.to_owned());
        let mut results = self.results.lock().unwrap();
        if results.len() > 1 { results.remove(0) } else { results[0].clone() }
    }
}

fn descriptor(environment_id: &str) -> EnvironmentProbeResult {
    EnvironmentProbeResult::Descriptor(Box::new(support::descriptor(environment_id)))
}

fn target(dev_url: Option<&str>) -> PairTarget {
    PairTarget {
        state: PairServerState {
            host: None,
            port: 3773,
            dev_url: dev_url.map(str::to_owned),
        },
        environment_id: "env-1".into(),
    }
}

#[test]
fn environment_responses_are_classified_three_ways() {
    let body = serde_json::to_vec(&support::descriptor("env-1")).unwrap();
    assert!(matches!(classify_environment_response(200, &body), EnvironmentProbeResult::Descriptor(_)));
    // A proxy answering for a backend that is gone is a stale mapping, not an occupant.
    for status in [502, 503, 504] {
        assert_eq!(classify_environment_response(status, b""), EnvironmentProbeResult::Unreachable);
    }
    for (status, body) in [(404, &b"{}"[..]), (500, b""), (200, b"<html>"), (200, b"{}"), (301, b"")] {
        assert_eq!(classify_environment_response(status, body), EnvironmentProbeResult::NotAT3Server, "{status}");
    }
}

#[tokio::test]
async fn the_http_prober_reads_the_well_known_descriptor_over_loopback() {
    let body = serde_json::to_value(support::descriptor("env-1")).unwrap();
    let server = FakeRelay::start(move |request| {
        if request.path == "/.well-known/t3/environment" {
            (200, body.clone())
        } else {
            (404, json!({}))
        }
    })
    .await;
    let prober = HttpEnvironmentProber::default();
    assert!(matches!(
        within(prober.probe(&format!("{}/pair/ignored", server.url))).await,
        EnvironmentProbeResult::Descriptor(_)
    ));
    assert_eq!(server.recorded()[0].path, "/.well-known/t3/environment");
    let nothing = FakeRelay::start(|_| (404, json!({}))).await;
    assert_eq!(within(prober.probe(&nothing.url)).await, EnvironmentProbeResult::NotAT3Server);
    let unavailable = FakeRelay::start(|_| (503, json!({}))).await;
    assert_eq!(within(prober.probe(&unavailable.url)).await, EnvironmentProbeResult::Unreachable);
    assert_eq!(
        within(prober.probe(&format!("http://127.0.0.1:{}", free_port()))).await,
        EnvironmentProbeResult::Unreachable
    );
}

#[tokio::test(start_paused = true)]
async fn an_unmapped_port_is_published_and_the_answer_awaited() {
    let tailscale = FakeTailscale::named(Some("desktop.tail.ts.net"));
    let prober = ScriptedProber::new(vec![
        EnvironmentProbeResult::Unreachable,
        EnvironmentProbeResult::Unreachable,
        descriptor("env-1"),
    ]);
    let base = within(resolve_tailscale_pairing_base(&tailscale, &prober, &target(None), 8443))
        .await
        .unwrap();
    assert_eq!(base.base_url, "https://desktop.tail.ts.net:8443/");
    assert_eq!(*tailscale.serves.lock().unwrap(), [(3773, 8443, None)]);
    assert_eq!(
        base.notes,
        ["Tailscale Serve now maps https://desktop.tail.ts.net:8443/ to this server and persists across restarts. Remove it with `tailscale serve --https=8443 off`."]
    );
    assert_eq!(prober.urls.lock().unwrap().len(), 3);

    // Never answering: the note says so, after five attempts.
    let prober = ScriptedProber::new(vec![EnvironmentProbeResult::Unreachable]);
    let base = within(resolve_tailscale_pairing_base(
        &FakeTailscale::named(Some("desktop.tail.ts.net")),
        &prober,
        &target(None),
        443,
    ))
    .await
    .unwrap();
    assert_eq!(base.base_url, "https://desktop.tail.ts.net/");
    assert!(base.notes[1].starts_with("The HTTPS endpoint has not answered yet."));
    assert_eq!(prober.urls.lock().unwrap().len(), 1 + 5);
}

#[tokio::test(start_paused = true)]
async fn an_existing_mapping_is_reused_only_for_this_environment() {
    let tailscale = FakeTailscale::named(Some("desktop.tail.ts.net"));
    let prober = ScriptedProber::new(vec![descriptor("env-1")]);
    let base = within(resolve_tailscale_pairing_base(&tailscale, &prober, &target(None), 443))
        .await
        .unwrap();
    assert!(base.notes.is_empty() && tailscale.serves.lock().unwrap().is_empty());

    // A dev server's mapping may front the backend: it is repointed at the web port.
    let dev = target(Some("http://localhost:5733/"));
    let base = within(resolve_tailscale_pairing_base(&tailscale, &prober, &dev, 443)).await.unwrap();
    assert_eq!(*tailscale.serves.lock().unwrap(), [(5733, 443, None)]);
    assert_eq!(base.notes.len(), 1);

    let other = ScriptedProber::new(vec![descriptor("someone-else")]);
    assert_eq!(
        within(resolve_tailscale_pairing_base(&tailscale, &other, &target(None), 443)).await.unwrap_err(),
        PairError::ServesOtherEnvironment { serve_port: 443 }
    );
    let occupied = ScriptedProber::new(vec![EnvironmentProbeResult::NotAT3Server]);
    assert_eq!(
        within(resolve_tailscale_pairing_base(&tailscale, &occupied, &target(None), 443)).await.unwrap_err(),
        PairError::ServePortOccupied { serve_port: 443 }
    );
    // The mapping turning out to front another environment after publishing is refused too.
    let swapped = ScriptedProber::new(vec![EnvironmentProbeResult::Unreachable, descriptor("someone-else")]);
    assert_eq!(
        within(resolve_tailscale_pairing_base(&tailscale, &swapped, &target(None), 443)).await.unwrap_err(),
        PairError::ServesOtherEnvironment { serve_port: 443 }
    );
}

#[tokio::test(start_paused = true)]
async fn tailscale_and_serve_failures_are_named() {
    let prober = ScriptedProber::new(vec![EnvironmentProbeResult::Unreachable]);
    let down = FakeTailscale {
        status: Err("spawn".into()),
        ..FakeTailscale::named(None)
    };
    assert!(matches!(
        within(resolve_tailscale_pairing_base(&down, &prober, &target(None), 443)).await.unwrap_err(),
        PairError::TailscaleUnavailable { .. }
    ));
    assert_eq!(
        within(resolve_tailscale_pairing_base(&FakeTailscale::named(None), &prober, &target(None), 443))
            .await
            .unwrap_err(),
        PairError::MagicDnsNameMissing
    );
    let failing = FakeTailscale {
        serve: Err("denied".into()),
        ..FakeTailscale::named(Some("desktop.tail.ts.net"))
    };
    let error = within(resolve_tailscale_pairing_base(&failing, &prober, &target(None), 8443)).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        "tailscale serve failed for HTTPS port 8443. Run `tailscale serve --https=8443 --bg <local-url>` by hand to see why."
    );
    // An https dev URL cannot be proxied: nothing is configured.
    let tailscale = FakeTailscale::named(Some("desktop.tail.ts.net"));
    let error = within(resolve_tailscale_pairing_base(
        &tailscale,
        &prober,
        &target(Some("https://localhost:5733/")),
        443,
    ))
    .await
    .unwrap_err();
    assert!(matches!(error, PairError::DevServerNotProxiable { .. }));
    assert!(tailscale.serves.lock().unwrap().is_empty());
}

/// The real `Tailscale` implements the trait: `serve` is invoked with the original's arguments.
#[tokio::test]
async fn the_tailscale_cli_backs_the_decisions() {
    let dir = temp_dir("pair-tailscale");
    let log = dir.path().join("log");
    let status = r#"{"Self":{"DNSName":"desktop.tail.ts.net.","TailscaleIPs":["100.100.100.100"]}}"#;
    let script = write_script(
        dir.path(),
        "tailscale",
        &format!("echo \"$*\" >> {}\ncase \"$1\" in status) echo '{status}' ;; esac", log.display()),
    );
    let tailscale = t3_remote::tailscale::Tailscale::with_executable(script.display().to_string());
    let prober = ScriptedProber::new(vec![EnvironmentProbeResult::Unreachable, descriptor("env-1")]);
    let mut state = target(None);
    state.state.host = Some("192.168.1.42".into());
    let base = within(resolve_tailscale_pairing_base(&tailscale, &prober, &state, 443)).await.unwrap();
    assert_eq!(base.base_url, "https://desktop.tail.ts.net/");
    let lines: Vec<String> = std::fs::read_to_string(log).unwrap().lines().map(str::to_owned).collect();
    assert_eq!(
        lines,
        ["status --json", "serve --bg --https=443 http://192.168.1.42:3773"]
    );
}
