//! The Tailscale and ssh helpers against fake executables at absolute temp paths. The real
//! `tailscale` and `ssh` are never run.
#![cfg(unix)]
mod support;

use std::time::Duration;

use support::*;
use t3_remote::ssh::*;
use t3_remote::tailscale::*;

const STATUS: &str =
    r#"{"Self":{"DNSName":"desktop.tail.ts.net.","TailscaleIPs":["100.100.100.100"]}}"#;

fn tailscale_at(dir: &tempfile::TempDir, body: &str) -> Tailscale {
    Tailscale::with_executable(
        write_script(dir.path(), "tailscale", body)
            .display()
            .to_string(),
    )
}

#[tokio::test]
async fn status_is_read_through_the_cli() {
    let dir = temp_dir("tailscale-status");
    let args_file = dir.path().join("args");
    let tailscale = tailscale_at(
        &dir,
        &format!(
            "echo \"$*\" > {}\ncat <<'EOF'\n{STATUS}\nEOF",
            args_file.display()
        ),
    );
    let status = within(tailscale.read_status()).await.unwrap();
    assert_eq!(
        status.magic_dns_name.as_deref(),
        Some("desktop.tail.ts.net")
    );
    assert_eq!(status.tailnet_ipv4_addresses, ["100.100.100.100"]);
    assert_eq!(
        std::fs::read_to_string(args_file).unwrap().trim(),
        "status --json"
    );
}

#[tokio::test]
async fn a_cli_that_cannot_run_or_prints_nonsense_is_a_typed_failure() {
    let dir = temp_dir("tailscale-failures");
    let missing = Tailscale::with_executable("/nonexistent/tailscale");
    assert_eq!(
        within(missing.read_status()).await.unwrap_err(),
        TailscaleReadError::Command(TailscaleError::Spawn {
            subcommand: "status"
        })
    );
    let garbage = tailscale_at(&dir, "echo '{not-json'");
    assert_eq!(
        within(garbage.read_status()).await.unwrap_err(),
        TailscaleReadError::Parse(StatusParseError)
    );
}

#[tokio::test]
async fn nonzero_exits_keep_structured_diagnostics_and_never_the_text() {
    let dir = temp_dir("tailscale-exit");
    let secret = "tskey-auth-secret-token-value";
    let tailscale = tailscale_at(
        &dir,
        &format!("printf '%s' 'not logged in {secret}' >&2\nexit 7"),
    );
    let error = within(tailscale.read_status()).await.unwrap_err();
    let TailscaleReadError::Command(error) = error else {
        panic!("{error:?}")
    };
    assert_eq!(
        error,
        TailscaleError::Exit {
            subcommand: "status",
            argument_count: 2,
            exit_code: 7,
            stdout_length: Some(0),
            stderr_length: 43,
            stderr_diagnostic: Some(StderrDiagnostic::NotLoggedIn),
        }
    );
    assert_eq!(error.to_string(), "tailscale status exited with code 7.");
    assert!(!format!("{error:?}").contains(secret));

    let novel = tailscale_at(
        &dir,
        &format!("echo 'something novel went wrong for node fluffy-badger {secret}' >&2\nexit 3"),
    );
    let error = within(novel.read_status()).await.unwrap_err();
    let rendered = format!("{error:?}");
    assert!(
        rendered.contains("Unknown")
            && !rendered.contains(secret)
            && !rendered.contains("fluffy-badger")
    );
}

#[tokio::test]
async fn status_gives_up_after_its_timeout() {
    let dir = temp_dir("tailscale-timeout");
    let mut tailscale = tailscale_at(&dir, "exec sleep 600");
    tailscale.status_timeout = Duration::from_millis(50);
    let error = within(tailscale.read_status()).await.unwrap_err();
    assert_eq!(
        error,
        TailscaleReadError::Command(TailscaleError::Timeout {
            subcommand: "status",
            timeout_ms: 50
        })
    );
    assert_eq!(error.to_string(), "tailscale status timed out after 50ms.");
}

#[tokio::test]
async fn serve_is_configured_and_removed_with_the_originals_arguments() {
    let dir = temp_dir("tailscale-serve");
    let log = dir.path().join("log");
    let tailscale = tailscale_at(&dir, &format!("echo \"$*\" >> {}", log.display()));
    within(tailscale.ensure_serve(13773, Some(8443), None))
        .await
        .unwrap();
    within(tailscale.ensure_serve(3773, None, Some("192.168.1.9")))
        .await
        .unwrap();
    within(tailscale.disable_serve(Some(8443))).await.unwrap();
    within(tailscale.disable_serve(None)).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(log)
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        [
            "serve --bg --https=8443 http://127.0.0.1:13773",
            "serve --bg --https=443 http://192.168.1.9:3773",
            "serve --https=8443 off",
            "serve --https=443 off",
        ]
    );
}

#[tokio::test]
async fn a_failing_serve_retains_its_diagnostic() {
    let dir = temp_dir("tailscale-serve-fail");
    let tailscale = tailscale_at(
        &dir,
        "printf '%s' 'serve permission denied tskey-auth-secret-token-value' >&2\nexit 1",
    );
    let error = within(tailscale.ensure_serve(13773, Some(8443), None))
        .await
        .unwrap_err();
    assert_eq!(
        error,
        TailscaleError::Exit {
            subcommand: "serve",
            argument_count: 4,
            exit_code: 1,
            stdout_length: None,
            stderr_length: 53,
            stderr_diagnostic: Some(StderrDiagnostic::PermissionDenied),
        }
    );
}

#[tokio::test]
async fn the_https_endpoint_is_probed_at_the_well_known_path() {
    let server = FakeRelay::start(|request| {
        if request.path == "/.well-known/t3/environment" {
            (200, serde_json::json!({}))
        } else {
            (404, serde_json::json!({}))
        }
    })
    .await;
    assert!(
        within(probe_tailscale_https_endpoint(
            &format!("{}/some/where", server.url),
            None
        ))
        .await
    );
    assert_eq!(server.recorded()[0].path, "/.well-known/t3/environment");
    let nothing = FakeRelay::start(|_| (404, serde_json::json!({}))).await;
    assert!(!within(probe_tailscale_https_endpoint(&nothing.url, None)).await);
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    assert!(
        !within(probe_tailscale_https_endpoint(
            &format!("http://127.0.0.1:{port}"),
            None
        ))
        .await
    );
    assert!(!probe_tailscale_https_endpoint("not a url", None).await);
}

// ---- ssh -----------------------------------------------------------------------------------------------

fn target() -> SshTarget {
    SshTarget {
        alias: "devbox".into(),
        hostname: "devbox.example.com".into(),
        username: Some("julius".into()),
        port: Some(2222),
    }
}

#[tokio::test]
async fn ssh_runs_with_the_originals_arguments_and_feeds_stdin() {
    let dir = temp_dir("ssh-run");
    let log = dir.path().join("log");
    let ssh = Ssh::with_executable(
        write_script(
            dir.path(),
            "ssh",
            &format!("echo \"$*\" > {}\ncat\necho err >&2", log.display()),
        )
        .display()
        .to_string(),
    );
    let result = within(ssh.run(
        &target(),
        RunSshOptions {
            remote_command_args: vec!["echo".into(), "hi".into()],
            stdin: Some("piped".into()),
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    assert_eq!(result.stdout, "piped");
    assert_eq!(result.stderr.trim(), "err");
    assert_eq!(
        std::fs::read_to_string(log).unwrap().trim(),
        "-o BatchMode=yes -o ConnectTimeout=10 -p 2222 julius@devbox echo hi"
    );
}

#[tokio::test]
async fn ssh_failures_prefer_stderr_then_redacted_stdout_then_a_default() {
    let dir = temp_dir("ssh-fail");
    let run = |body: &str| {
        let ssh = Ssh::with_executable(write_script(dir.path(), "ssh", body).display().to_string());
        async move {
            within(ssh.run(&target(), RunSshOptions::default()))
                .await
                .unwrap_err()
        }
    };
    let SshError::Command {
        message, exit_code, ..
    } = run("echo 'Permission denied (publickey).' >&2\nexit 255").await
    else {
        panic!()
    };
    assert_eq!(
        (message.as_str(), exit_code),
        ("Permission denied (publickey).", Some(255))
    );
    let SshError::Command {
        message, stdout, ..
    } = run(r#"echo '{"credential": "pairing-token", "error": "boom"}'; exit 1"#).await
    else {
        panic!()
    };
    assert_eq!(message, r#"{"credential": "[redacted]", "error": "boom"}"#);
    assert!(!stdout.contains("pairing-token"));
    let SshError::Command { message, .. } = run("exit 4").await else {
        panic!()
    };
    assert_eq!(message, "SSH command failed for julius@devbox (exit 4).");
}

#[tokio::test]
async fn ssh_that_never_finishes_times_out_and_missing_ssh_is_reported() {
    let dir = temp_dir("ssh-timeout");
    let ssh = Ssh::with_executable(
        write_script(dir.path(), "ssh", "exec sleep 600")
            .display()
            .to_string(),
    );
    let error = within(ssh.run(
        &target(),
        RunSshOptions {
            timeout: Some(Duration::from_millis(50)),
            ..Default::default()
        },
    ))
    .await
    .unwrap_err();
    assert_eq!(error.to_string(), "SSH command timed out after 50ms.");
    let missing = Ssh::with_executable("/nonexistent/ssh");
    assert!(matches!(
        within(missing.run(&target(), RunSshOptions::default()))
            .await
            .unwrap_err(),
        SshError::Command {
            exit_code: None,
            ..
        }
    ));
    let nameless = SshTarget {
        alias: " ".into(),
        hostname: " ".into(),
        username: None,
        port: None,
    };
    assert_eq!(
        within(missing.run(&nameless, RunSshOptions::default()))
            .await
            .unwrap_err()
            .to_string(),
        "SSH target is missing its alias/hostname."
    );
}

#[tokio::test]
async fn aliases_are_resolved_through_ssh_g_and_fall_back_to_themselves() {
    let dir = temp_dir("ssh-resolve");
    let log = dir.path().join("log");
    let ssh = Ssh::with_executable(
        write_script(dir.path(), "ssh", &format!("echo \"$*\" > {}\nprintf 'hostname devbox.example.com\\nuser julius\\nport 2222\\n'", log.display())).display().to_string(),
    );
    let resolved = within(ssh.resolve_target(" devbox ")).await.unwrap();
    assert_eq!(resolved, target());
    assert_eq!(
        std::fs::read_to_string(log).unwrap().trim(),
        "-o BatchMode=yes -o ConnectTimeout=10 -G devbox"
    );
    let failing = Ssh::with_executable(
        write_script(dir.path(), "ssh2", "exit 255")
            .display()
            .to_string(),
    );
    assert_eq!(
        within(failing.resolve_target("alias")).await.unwrap(),
        SshTarget {
            alias: "alias".into(),
            hostname: "alias".into(),
            username: None,
            port: None
        }
    );
    assert_eq!(
        within(failing.resolve_target("  "))
            .await
            .unwrap_err()
            .message,
        "SSH host alias is required."
    );
}
