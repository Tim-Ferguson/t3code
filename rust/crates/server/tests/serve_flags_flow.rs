//! The flags and the positional of `serve` that the original has: the working directory (made when it is missing), the project and
//! first thread registered for it (`--auto-bootstrap-project-from-cwd`), the dev URL loopback requests for the page are handed to
//! (`--dev-url`), and the request log (`--log-websocket-events`). The real binary, in a temporary state directory with an empty
//! environment; no provider CLI is started and nothing outside the temporary directory is touched.
#![cfg(unix)]
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    process::{Child, Command, Stdio},
    sync::mpsc,
};
use t3_server::test_support::HANG_BOUND;

struct Server {
    child: Child,
    ready: Value,
    _lines: mpsc::Receiver<String>,
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start(root: &Path, state: &Path, args: &[&str], env: &[(&str, &str)]) -> Server {
    let mut settings = serde_json::to_value(t3_contracts::ServerSettings::default()).unwrap();
    for (_, config) in settings["providers"].as_object_mut().unwrap() {
        config["enabled"] = json!(false);
    }
    let settings_path = root.join("settings.json");
    std::fs::write(&settings_path, serde_json::to_vec(&settings).unwrap()).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_t3-server"));
    command.env_clear().env("PATH", "/usr/bin:/bin").env("HOME", root).env("RUST_LOG", "t3_server=info");
    for (key, value) in env {
        command.env(key, value);
    }
    command.arg("serve").args(args).args(["--port", "0", "--state-dir"]).arg(state).arg("--settings").arg(&settings_path);
    let mut child = command.stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
    let output = child.stdout.take().unwrap();
    let (lines_in, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            let Ok(line) = line else { break };
            if lines_in.send(line).is_err() {
                break;
            }
        }
    });
    let ready = loop {
        let line = lines.recv_timeout(HANG_BOUND).expect("the server printed its readiness line");
        if let Some(rest) = line.strip_prefix("T3_SERVER_READY ") {
            break serde_json::from_str::<Value>(rest).unwrap();
        }
    };
    Server { child, ready, _lines: lines }
}

fn stop(mut server: Server) {
    assert_eq!(unsafe { libc::kill(server.child.id() as i32, libc::SIGTERM) }, 0);
    assert!(server.child.wait().unwrap().success());
}

fn get(port: u64, path: &str, host: &str) -> (u16, String, String) {
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port as u16)).unwrap();
    stream.set_read_timeout(Some(HANG_BOUND)).unwrap();
    write!(stream, "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").unwrap();
    let mut text = String::new();
    stream.read_to_string(&mut text).unwrap();
    let status = text.split(' ').nth(1).unwrap().parse().unwrap();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    (status, head.to_owned(), body.to_owned())
}

#[test]
fn the_positional_names_the_working_directory_and_the_flag_registers_a_project_for_it() {
    let root = tempfile::Builder::new().prefix("t3code-rust-port-test-serve-").tempdir().unwrap();
    let root_path = std::fs::canonicalize(root.path()).unwrap();
    let state = root_path.join("state");
    let workspace = root_path.join("work/space");
    assert!(!workspace.exists());
    let server = start(&root_path, &state, &[workspace.to_str().unwrap(), "--auto-bootstrap-project-from-cwd"], &[]);
    // The directory is made when it is missing, as the original makes it.
    assert!(workspace.is_dir());
    stop(server);

    let store = t3_server::persistence::Store::open(state.join("rust-state.sqlite")).unwrap();
    let projects = t3_server::project::ProjectService::new(store.clone()).list().unwrap();
    assert_eq!(projects.len(), 1, "{projects:?}");
    assert_eq!(projects[0]["workspaceRoot"], std::fs::canonicalize(&workspace).unwrap().to_str().unwrap());
    assert_eq!(projects[0]["title"], "space");
    let shell = t3_server::thread::ThreadService::new(store).shell_snapshot().unwrap();
    let threads = shell["threads"].as_array().unwrap();
    assert_eq!(threads.len(), 1);
    assert_eq!((threads[0]["title"].as_str(), threads[0]["projectId"].clone()), (Some("New thread"), projects[0]["id"].clone()));

    // A second start finds both and adds nothing; the environment variable asks for the same thing.
    let server = start(&root_path, &state, &[workspace.to_str().unwrap()], &[("T3CODE_AUTO_BOOTSTRAP_PROJECT_FROM_CWD", "true")]);
    stop(server);
    let store = t3_server::persistence::Store::open(state.join("rust-state.sqlite")).unwrap();
    assert_eq!(t3_server::project::ProjectService::new(store.clone()).list().unwrap().len(), 1);
    assert_eq!(t3_server::thread::ThreadService::new(store).shell_snapshot().unwrap()["threads"].as_array().unwrap().len(), 1);

    // Without the flag nothing is registered.
    let other = root_path.join("other-state");
    let server = start(&root_path, &other, &[workspace.to_str().unwrap()], &[]);
    stop(server);
    let store = t3_server::persistence::Store::open(other.join("rust-state.sqlite")).unwrap();
    assert!(t3_server::project::ProjectService::new(store).list().unwrap().is_empty());
}

#[test]
fn a_dev_url_takes_the_page_for_loopback_names_only() {
    let root = tempfile::Builder::new().prefix("t3code-rust-port-test-serve-").tempdir().unwrap();
    let root_path = std::fs::canonicalize(root.path()).unwrap();
    let state = root_path.join("state");
    let server = start(&root_path, &state, &["--dev-url", "http://127.0.0.1:5733"], &[]);
    let port = server.ready["port"].as_u64().unwrap();
    let (status, head, _) = get(port, "/threads/x?y=1", "localhost:3773");
    assert_eq!(status, 302);
    assert!(head.to_lowercase().contains("location: http://127.0.0.1:5733/threads/x?y=1"), "{head}");
    // The API stays here, and a name that is not loopback is served the app (there is none to serve in this bundle-less start).
    assert_eq!(get(port, "/.well-known/t3/environment", "localhost").0, 200);
    assert_eq!(get(port, "/api/nothing", "localhost").0, 404);
    assert_eq!(get(port, "/", "box.example").0, 503);
    stop(server);

    // The environment names the dev server too.
    let server = start(&root_path, &state, &[], &[("VITE_DEV_SERVER_URL", "http://127.0.0.1:5744/")]);
    let port = server.ready["port"].as_u64().unwrap();
    assert_eq!(get(port, "/", "127.0.0.1").0, 302);
    stop(server);
}

#[test]
fn an_extra_positional_is_refused() {
    let root = tempfile::Builder::new().prefix("t3code-rust-port-test-serve-").tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_t3-server")).env_clear().args(["serve", "one", "two", "--state-dir"]).arg(root.path().join("state")).output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Unexpected argument: two"), "{}", String::from_utf8_lossy(&output.stderr));
}
