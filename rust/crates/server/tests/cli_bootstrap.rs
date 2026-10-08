//! Exercise the CLI's real inherited supervisor channel before runtime startup.
#[cfg(unix)]
#[tokio::test]
async fn inherited_bootstrap_drives_desktop_auth_telemetry_and_home_without_secret_output() {
    use serde_json::{Value, json};
    use std::{
        io::BufRead,
        os::{
            fd::AsRawFd,
            unix::{net::UnixStream, process::CommandExt},
        },
        process::{Command, Stdio},
    };
    use tokio::io::{AsyncBufReadExt, BufReader};
    struct OwnedChild(std::process::Child);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let root = tempfile::tempdir().unwrap();
    let settings = root.path().join("settings.json");
    let mut defaults = serde_json::to_value(t3_contracts::ServerSettings::default()).unwrap();
    for (_, config) in defaults["providers"].as_object_mut().unwrap() {
        config["enabled"] = json!(false);
    }
    std::fs::write(&settings, serde_json::to_vec(&defaults).unwrap()).unwrap();
    let (server, host) = UnixStream::pair().unwrap();
    let telemetry_fd = server.as_raw_fd();
    let envelope = json!({"mode":"desktop","noBrowser":true,"port":3773,"host":"127.0.0.1","t3Home":root.path(),"desktopBootstrapToken":"fixture-trusted-IPC-token","tailscaleServeEnabled":false,"tailscaleServePort":443,"desktopTelemetryFd":telemetry_fd,"desktopTelemetryControlFd":telemetry_fd});
    let path = root.path().join("bootstrap");
    std::fs::write(&path, format!("{envelope}\n")).unwrap();
    let input = std::fs::File::open(path).unwrap();
    let bootstrap_fd = input.as_raw_fd();
    let mut command = Command::new(env!("CARGO_BIN_EXE_t3-server"));
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root.path())
        .env("RUST_LOG", "t3_server=info")
        .env("T3CODE_BOOTSTRAP_FD", bootstrap_fd.to_string())
        .args(["serve", "--port", "0", "--settings"])
        .arg(&settings)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    unsafe {
        command.pre_exec(move || {
            for fd in [bootstrap_fd, telemetry_fd] {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let mut child = OwnedChild(command.spawn().unwrap());
    drop(input);
    drop(server);
    let output = child.0.stdout.take().unwrap();
    let errors = child.0.stderr.take().unwrap();
    let (ready, receive) = tokio::sync::oneshot::channel();
    let logger = std::thread::spawn(move || {
        let mut ready = Some(ready);
        let mut lines = Vec::new();
        for line in std::io::BufReader::new(output).lines() {
            let line = line.unwrap();
            if let Some(start) = line.find("127.0.0.1:") {
                let address = line[start..]
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || matches!(c, '.' | ':'))
                    .collect::<String>();
                if let Some(ready) = ready.take() {
                    ready.send(address).ok();
                }
            }
            lines.push(line);
        }
        lines.join("\n")
    });
    let stderr = std::thread::spawn(move || std::io::read_to_string(errors).unwrap());
    host.set_nonblocking(true).unwrap();
    let host = tokio::net::UnixStream::from_std(host).unwrap();
    let mut control = BufReader::new(host);
    let mut frame = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        control.read_line(&mut frame),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&frame).unwrap()["type"],
        "setHostPowerIntervals"
    );
    let address = tokio::time::timeout(std::time::Duration::from_secs(20), receive)
        .await
        .unwrap()
        .unwrap();
    let http = reqwest::Client::new();
    let base = format!("http://{address}");
    let session: Value = http
        .get(format!("{base}/api/auth/session"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(session["auth"]["policy"], "desktop-managed-local");
    assert_eq!(
        session["auth"]["bootstrapMethods"],
        json!(["desktop-bootstrap"])
    );
    for _ in 0..2 {
        let response=http.post(format!("{base}/oauth/token")).header("content-type","application/x-www-form-urlencoded").body("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Atoken-exchange&subject_token=fixture-trusted-IPC-token&subject_token_type=urn%3At3%3Aparams%3Aoauth%3Atoken-type%3Aenvironment-bootstrap&requested_token_type=urn%3Aietf%3Aparams%3Aoauth%3Atoken-type%3Aaccess_token&scope=orchestration%3Aread").send().await.unwrap();
        assert!(response.status().is_success(), "{}", response.status());
        let token: Value = response.json().await.unwrap();
        let state: Value = http
            .get(format!("{base}/api/auth/session"))
            .bearer_auth(token["access_token"].as_str().unwrap())
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(state["authenticated"], true);
    }
    assert!(root.path().join("userdata/rust-state.sqlite").exists());
    unsafe {
        libc::kill(child.0.id() as i32, libc::SIGINT);
    }
    let status = tokio::task::spawn_blocking(move || child.0.wait().unwrap())
        .await
        .unwrap();
    assert!(status.success());
    let output = logger.join().unwrap();
    let errors = stderr.join().unwrap();
    assert!(!output.contains("fixture-trusted-IPC-token"));
    assert!(!errors.contains("fixture-trusted-IPC-token"));
}
