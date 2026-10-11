//! The commands of `t3-server` besides the server (`apps/server/src/cli/*.ts`), run as the real binary in an isolated temp home with an
//! empty environment: `--version`, the hidden `__ssh-helper` and `__service-preflight` the remote SSH launch depends on,
//! `pair`, `auth pairing|session`, `project add|remove|rename` (on the state itself and against a running server), and
//! `pair --tailscale` over fakes. What the commands print is held to the unchanged original's output
//! (`tools/generate_cli_fixtures.mjs`). No network beyond loopback, no `ssh`, no `tailscale`, nothing outside the temp directory.
#![cfg(unix)]
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
};
use t3_server::test_support::HANG_BOUND;

struct Home {
    directory: tempfile::TempDir,
    root: PathBuf,
}
fn home() -> Home {
    let directory = tempfile::Builder::new().prefix("t3code-rust-port-test-cli-").tempdir().unwrap();
    let root = std::fs::canonicalize(directory.path()).unwrap();
    Home { directory, root }
}
impl Home {
    fn state(&self) -> PathBuf {
        self.root.join("home").join("userdata")
    }
    fn base(&self) -> PathBuf {
        self.root.join("home")
    }
    fn command(&self, path: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_t3-server"));
        command.env_clear().env("PATH", path).env("HOME", &self.root).current_dir(&self.root);
        command
    }
    fn run(&self, args: &[&str]) -> Output {
        self.run_with_path("/usr/bin:/bin", args)
    }
    fn run_with_path(&self, path: &str, args: &[&str]) -> Output {
        let output = self.command(path).args(args).output().unwrap();
        Output { stdout: String::from_utf8(output.stdout).unwrap(), stderr: String::from_utf8(output.stderr).unwrap(), code: output.status.code().unwrap_or(-1) }
    }
    /// A path of this home's own, as an argument.
    fn arg(&self, relative: &str) -> String {
        self.root.join(relative).to_string_lossy().into_owned()
    }
    fn base_arg(&self) -> String {
        self.base().to_string_lossy().into_owned()
    }
}
#[derive(Debug)]
struct Output {
    stdout: String,
    stderr: String,
    code: i32,
}
impl Output {
    fn ok(&self) -> &str {
        assert_eq!(self.code, 0, "stderr: {}", self.stderr);
        &self.stdout
    }
    fn json(&self) -> Value {
        serde_json::from_str(self.ok()).unwrap_or_else(|error| panic!("{error}: {}", self.stdout))
    }
}

fn fixture_rows(kind: &str) -> Vec<Value> {
    include_str!("fixtures/cli-output.jsonl").lines().map(|line| serde_json::from_str::<Value>(line).unwrap()).filter(|row| row["kind"] == kind).collect()
}

/// The auth service over a state directory, as the server would open it, to look at what a command left.
fn auth_over(state: &Path) -> t3_server::auth::AuthService {
    let store = t3_server::persistence::Store::open(state.join("rust-state.sqlite")).unwrap();
    let secret: [u8; 32] = std::fs::read(state.join("rust-signing-secret")).unwrap().try_into().unwrap();
    t3_server::auth::AuthService::new(store, secret, "t3_session".into(), "loopback-browser".into()).unwrap()
}

// ---------------------------------------------------------------------------------------------
// --version and the hidden helpers
// ---------------------------------------------------------------------------------------------

#[test]
fn version_prints_the_name_and_a_v_prefixed_version_the_remote_launch_reads_back() {
    let home = home();
    for flag in ["--version", "-v"] {
        let output = home.run(&[flag]);
        assert_eq!(output.ok(), format!("t3 v{}\n", env!("CARGO_PKG_VERSION")));
        // `/\bv(\S+)\s*$/` over standard output: the version after the last `v`-word.
        let reported = output.stdout.trim_end().rsplit(char::is_whitespace).next().and_then(|word| word.strip_prefix('v')).unwrap();
        assert_eq!(reported, env!("CARGO_PKG_VERSION"));
    }
}

#[test]
fn the_ssh_helpers_answer_the_launch_script_in_its_own_terms() {
    let home = home();
    // pick-port: the preferred port from the file when free, else the next free one; no newline, nothing else.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let busy = listener.local_addr().unwrap().port();
    let port_file = home.root.join("port");
    std::fs::write(&port_file, format!("{busy}\n")).unwrap();
    let picked = home.run(&["__ssh-helper", "pick-port", &port_file.to_string_lossy(), "3773", "50"]);
    let chosen: u16 = picked.ok().parse().unwrap();
    assert!(chosen != busy && chosen > busy && chosen < busy.saturating_add(50), "{chosen} after {busy}");
    assert!(!picked.stdout.ends_with('\n'));
    // Without a file the default is where the scan starts; no free port in a window of nothing is exit 1 and no output.
    let default = home.run(&["__ssh-helper", "pick-port", &home.arg("missing"), &busy.to_string(), "1"]);
    assert_eq!((default.stdout.as_str(), default.code), ("", 1));
    let fallback = home.run(&["__ssh-helper", "pick-port", &home.arg("missing"), &chosen.to_string(), "1"]);
    assert_eq!(fallback.ok(), chosen.to_string());

    // wait-ready: exit 0 once the loopback server answers 2xx, 1 at the deadline.
    let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let served = server.local_addr().unwrap().port();
    let answering = std::thread::spawn(move || {
        use std::io::{Read, Write};
        if let Ok((mut stream, _)) = server.accept() {
            let mut buffer = [0u8; 1024];
            let _ = stream.read(&mut buffer);
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok");
        }
    });
    assert_eq!(home.run(&["__ssh-helper", "wait-ready", &served.to_string(), "5000", "1000"]).code, 0);
    answering.join().unwrap();
    // The port that answered is closed now: nothing is ready there.
    let closed = home.run(&["__ssh-helper", "wait-ready", &served.to_string(), "300", "100"]);
    assert_eq!((closed.stdout.as_str(), closed.code), ("", 1));

    // runtime-port: `<pid> <port>` for a live server recorded on a loopback origin, else exit 1.
    let runtime = home.root.join("server-runtime.json");
    let live = std::process::id();
    std::fs::write(&runtime, json!({"version":1,"pid":live,"host":"127.0.0.1","port":4242,"origin":"http://127.0.0.1:4242","startedAt":"2026-10-11T12:00:00.000Z"}).to_string()).unwrap();
    assert_eq!(home.run(&["__ssh-helper", "runtime-port", &runtime.to_string_lossy()]).ok(), format!("{live} 4242"));
    for (record, why) in [
        (json!({"pid":live,"port":4242,"origin":"https://127.0.0.1:4242"}), "https origin"),
        (json!({"pid":live,"port":4242,"origin":"http://example.com:4242"}), "foreign host"),
        (json!({"pid":0,"port":4242,"origin":"http://127.0.0.1:4242"}), "no pid"),
        (json!({"pid":live,"port":"x","origin":"http://127.0.0.1:4242"}), "no port"),
        (json!({"pid":999_999_999,"port":4242,"origin":"http://127.0.0.1:4242"}), "dead pid"),
    ] {
        std::fs::write(&runtime, record.to_string()).unwrap();
        let output = home.run(&["__ssh-helper", "runtime-port", &runtime.to_string_lossy()]);
        assert_eq!((output.stdout.as_str(), output.code), ("", 1), "{why}");
    }
    assert_eq!(home.run(&["__ssh-helper", "runtime-port", &home.arg("none")]).code, 1);
}

#[test]
fn the_service_preflight_prints_one_json_line_for_the_staged_update_check() {
    let home = home();
    let version = env!("CARGO_PKG_VERSION");
    let ready = home.run(&["__service-preflight", "--database-path", &home.arg("db.sqlite"), "--launcher-protocol", &t3_remote::service_protocol::SERVICE_LAUNCHER_PROTOCOL.to_string()]);
    assert_eq!(ready.ok(), format!("{}\n", json!({"status":"ready","version":version,"launcherProtocol":t3_remote::service_protocol::SERVICE_LAUNCHER_PROTOCOL})));
    let blocked = home.run(&["__service-preflight", "--database-path", "x", "--launcher-protocol", "9999"]);
    assert_eq!(blocked.ok(), format!("{}\n", json!({"status":"blocked","version":version,"reason":"This release requires a newer T3 Code service launcher. Update it on the server machine."})));
    // The caller (the update) reads the line back with the decoder it has.
    let decoded = t3_remote::service_preflight::decode_service_preflight_result(&serde_json::from_str(ready.stdout.trim()).unwrap()).unwrap();
    assert_eq!(decoded.version(), version);
    assert_ne!(home.run(&["__service-preflight", "--launcher-protocol", "1"]).code, 0);
}

// ---------------------------------------------------------------------------------------------
// auth
// ---------------------------------------------------------------------------------------------

#[test]
fn pairing_credentials_are_issued_listed_and_revoked_in_the_originals_words() {
    let home = home();
    let base = home.base_arg();
    // The call the remote launch makes.
    let issued = home.run(&["auth", "pairing", "create", "--base-dir", &base, "--json"]).json();
    let credential = issued["credential"].as_str().unwrap().to_owned();
    assert_eq!(credential.len(), 12);
    let standard: Vec<&str> = t3_contracts::AUTH_STANDARD_CLIENT_SCOPES.iter().map(|scope| serde_json::to_value(scope).unwrap().as_str().unwrap().to_owned()).map(|scope| &*Box::leak(scope.into_boxed_str())).collect();
    assert_eq!(issued["scopes"], json!(standard));
    assert!(issued.get("label").is_none() && issued.get("pairUrl").is_none());
    assert_eq!(issued.as_object().unwrap().keys().collect::<Vec<_>>(), ["id", "credential", "scopes", "expiresAt"]);

    // With scopes, a label, a lifetime and a base URL.
    let scoped = home.run(&["auth", "pairing", "create", "--base-dir", &base, "--scope", "orchestration:read", "--scope", "orchestration:read", "--scope", "access:write", "--ttl", "2h", "--label", "laptop", "--base-url", "https://box.example.ts.net/ignored?token=old", "--json"]).json();
    assert_eq!(scoped["scopes"], json!(["orchestration:read", "access:write"]));
    assert_eq!(scoped["label"], "laptop");
    let pair_url = format!("https://box.example.ts.net/pair#token={}", scoped["credential"].as_str().unwrap());
    assert_eq!(scoped["pairUrl"], pair_url.as_str());
    let lifetime = chrono::DateTime::parse_from_rfc3339(scoped["expiresAt"].as_str().unwrap()).unwrap().signed_duration_since(chrono::Utc::now());
    assert!(lifetime > chrono::Duration::minutes(119) && lifetime <= chrono::Duration::hours(2), "{lifetime}");

    // The credential is the server's: it exchanges for a session with exactly those scopes, once.
    let auth = auth_over(&home.state());
    let (session, _) = auth.exchange_pairing_bearer(scoped["credential"].as_str().unwrap(), None, json!({}), chrono::Utc::now()).unwrap();
    assert_eq!(session.scopes, vec![t3_contracts::AuthEnvironmentScope::OrchestrationRead, t3_contracts::AuthEnvironmentScope::AccessWrite]);
    assert!(auth.exchange_pairing_bearer(scoped["credential"].as_str().unwrap(), None, json!({}), chrono::Utc::now()).is_err());

    // Text and the list.
    let text = home.run(&["auth", "pairing", "create", "--base-dir", &base]);
    assert!(text.ok().starts_with("Issued client pairing token ") && text.stdout.contains("\nToken: ") && text.stdout.contains("\nExpires at: DateTime.Utc("), "{}", text.stdout);
    assert!(text.stdout.ends_with("\n\n"), "Console.log adds a line to text that ends with one");
    let list = home.run(&["auth", "pairing", "list", "--base-dir", &base, "--json"]).json();
    let ids: Vec<&str> = list.as_array().unwrap().iter().map(|link| link["id"].as_str().unwrap()).collect();
    assert_eq!(ids.len(), 2, "the used credential is gone: {list}");
    assert!(list[1]["label"].is_null() && list.as_array().unwrap().iter().all(|link| link["scopes"].is_array()));
    let readable = home.run(&["auth", "pairing", "list", "--base-dir", &base]);
    assert!(readable.ok().contains("\n  scopes: orchestration:read ") && readable.stdout.contains("\n  created: "), "{}", readable.stdout);

    let revoked = home.run(&["auth", "pairing", "revoke", "--base-dir", &base, ids[0]]);
    assert_eq!(revoked.ok(), format!("Revoked pairing credential {}.\n\n", ids[0]));
    assert_eq!(home.run(&["auth", "pairing", "revoke", "--base-dir", &base, ids[0]]).ok(), format!("No active pairing credential found for {}.\n\n", ids[0]));
    assert_eq!(home.run(&["auth", "pairing", "list", "--base-dir", &base, "--json"]).json().as_array().unwrap().len(), 1);

    // What it refuses.
    for (args, message) in [
        (vec!["--scope", "review:write"], "Invalid value for flag --scope"),
        (vec!["--scope", "nonsense"], "Invalid value for flag --scope"),
        (vec!["--ttl", "soon"], "Invalid value for flag --ttl"),
        (vec!["--ttl", "0m"], "Invalid value for flag --ttl"),
        (vec!["--bogus"], "Unrecognized flag: --bogus"),
        (vec!["extra"], "Unexpected argument: extra"),
    ] {
        let mut full = vec!["auth", "pairing", "create", "--base-dir", &base];
        full.extend(args);
        let output = home.run(&full);
        assert_eq!((output.code, output.stdout.as_str()), (1, ""), "{message}");
        assert!(output.stderr.contains(message), "{}", output.stderr);
    }
}

#[test]
fn bearer_sessions_are_issued_listed_and_revoked() {
    let home = home();
    let base = home.base_arg();
    let issued = home.run(&["auth", "session", "issue", "--base-dir", &base, "--label", "ci runner", "--json"]).json();
    assert_eq!(issued["method"], "bearer-access-token");
    assert_eq!(issued["subject"], "cli-issued-session");
    assert_eq!(issued["client"], json!({"label":"ci runner","deviceType":"bot"}));
    assert_eq!(issued["scopes"].as_array().unwrap().len(), t3_contracts::AUTH_ADMINISTRATIVE_SCOPES.len());
    assert_eq!(issued.as_object().unwrap().keys().collect::<Vec<_>>(), ["sessionId", "token", "method", "scopes", "subject", "client", "expiresAt"]);
    let token = issued["token"].as_str().unwrap().to_owned();
    // It is a session the server verifies, with thirty days to run.
    let session = auth_over(&home.state()).verify_session(&token, chrono::Utc::now()).unwrap();
    assert_eq!(session.session_id, issued["sessionId"]);
    let remaining = session.expires_at - chrono::Utc::now();
    assert!(remaining > chrono::Duration::days(29) && remaining <= chrono::Duration::days(30));

    let only = home.run(&["auth", "session", "issue", "--base-dir", &base, "--token-only", "--scope", "orchestration:read", "--subject", "script", "--ttl", "1h"]);
    let only_token = only.ok().trim_end().to_owned();
    assert_eq!(only.stdout, format!("{only_token}\n\n"));
    let narrowed = auth_over(&home.state()).verify_session(&only_token, chrono::Utc::now()).unwrap();
    assert_eq!((narrowed.subject.as_str(), narrowed.scopes.len()), ("script", 1));

    let text = home.run(&["auth", "session", "issue", "--base-dir", &base, "--label", "ci runner"]);
    assert!(text.ok().contains("\nSubject: cli-issued-session\nClient: ci runner | bot\nExpires at: 20"), "{}", text.stdout);

    let list = home.run(&["auth", "session", "list", "--base-dir", &base, "--json"]).json();
    assert_eq!(list.as_array().unwrap().len(), 3);
    assert!(list.as_array().unwrap().iter().all(|session| session["connected"] == false && session["lastConnectedAt"].is_null()));
    let readable = home.run(&["auth", "session", "list", "--base-dir", &base]);
    assert!(readable.ok().contains("\n  last connected: never\n") && readable.stdout.contains("  client: ci runner | bot\n"), "{}", readable.stdout);

    let id = issued["sessionId"].as_str().unwrap();
    assert_eq!(home.run(&["auth", "session", "revoke", "--base-dir", &base, id]).ok(), format!("Revoked session {id}.\n\n"));
    assert_eq!(home.run(&["auth", "session", "revoke", "--base-dir", &base, id]).ok(), format!("No active session found for {id}.\n\n"));
    assert!(auth_over(&home.state()).verify_session(&token, chrono::Utc::now()).is_err());
    assert_eq!(home.run(&["auth", "session", "list", "--base-dir", &base, "--json"]).json().as_array().unwrap().len(), 2);
}

#[test]
fn the_location_flags_choose_the_state_the_way_serve_does() {
    let home = home();
    // --state-dir names the directory itself, --base-dir its parent's `userdata`, and T3CODE_HOME the same as --base-dir.
    let direct = home.root.join("direct");
    home.run(&["auth", "pairing", "create", "--state-dir", &direct.to_string_lossy()]).ok();
    assert!(direct.join("rust-state.sqlite").exists());
    let mut with_env = home.command("/usr/bin:/bin");
    with_env.env("T3CODE_HOME", home.root.join("from-env"));
    assert_eq!(with_env.args(["auth", "pairing", "list", "--json"]).output().unwrap().status.code(), Some(0));
    assert!(home.root.join("from-env").join("userdata").join("rust-state.sqlite").exists());
    // Without any, `.t3-rust` under the working directory, where `serve` keeps its state too.
    home.run(&["auth", "pairing", "create"]).ok();
    assert!(home.root.join(".t3-rust").join("rust-state.sqlite").exists());
    // A directory that holds the original's database and none of this server's is not made a store of: the first start imports it.
    let original = home.root.join("original");
    std::fs::create_dir_all(&original).unwrap();
    std::fs::write(original.join("statev2.sqlite"), b"").unwrap();
    let refused = home.run(&["auth", "pairing", "create", "--state-dir", &original.to_string_lossy()]);
    assert_eq!(refused.code, 1);
    assert!(refused.stderr.contains("holds the original server's state"), "{}", refused.stderr);
    assert!(!original.join("rust-state.sqlite").exists());
    let _ = &home.directory;
}

// ---------------------------------------------------------------------------------------------
// The output, against the unchanged original's
// ---------------------------------------------------------------------------------------------

#[test]
fn what_the_auth_commands_print_is_what_the_original_prints() {
    use t3_contracts::AuthEnvironmentScope as Scope;
    let scope = |name: &str| -> Scope { serde_json::from_value(json!(name)).unwrap() };
    let time = |text: &str| chrono::DateTime::parse_from_rfc3339(text).unwrap().with_timezone(&chrono::Utc);
    let mut checked = 0;
    for row in fixture_rows("issuedPairing") {
        let input = &row["input"];
        let issued = t3_server::auth::IssuedPairingLink {
            id: input["id"].as_str().unwrap().into(),
            credential: input["credential"].as_str().unwrap().into(),
            label: input["label"].as_str().map(str::to_owned),
            created_at: time("2026-10-11T12:00:00.000Z"),
            expires_at: time(input["expiresAt"].as_str().unwrap()),
        };
        let scopes: Vec<Scope> = input["scopes"].as_array().unwrap().iter().map(|name| scope(name.as_str().unwrap())).collect();
        let mut got = t3_server::cli_commands::format_issued_pairing_credential(&issued, &scopes, input["json"] == true, input["baseUrl"].as_str()).unwrap();
        let mut expected = row["output"].as_str().unwrap().to_owned();
        // The text form interpolates the expiry as an object (`DateTime.Utc(<iso>)`) in the original: the bare time is what the
        // JSON carries, and the one difference the test accepts is that wrapper.
        if input["json"] != true {
            expected = expected.replace(&format!("Expires at: DateTime.Utc({})", input["expiresAt"].as_str().unwrap()), "Expires at: X");
            got = got.replace(&format!("Expires at: DateTime.Utc({})", input["expiresAt"].as_str().unwrap()), "Expires at: X");
        }
        assert_eq!(got, expected, "{input}");
        checked += 1;
    }
    for row in fixture_rows("pairingList") {
        assert_eq!(t3_server::cli_commands::format_pairing_credential_list(row["rows"].as_array().unwrap(), row["json"] == true), row["output"].as_str().unwrap(), "{row}");
        checked += 1;
    }
    for row in fixture_rows("issuedSession") {
        assert_eq!(t3_server::cli_commands::format_issued_session(&row["session"], row["json"] == true, row["tokenOnly"] == true), row["output"].as_str().unwrap(), "{row}");
        checked += 1;
    }
    for row in fixture_rows("sessionList") {
        assert_eq!(t3_server::cli_commands::format_session_list(row["rows"].as_array().unwrap(), row["json"] == true), row["output"].as_str().unwrap(), "{row}");
        checked += 1;
    }
    for row in fixture_rows("pairingUrl") {
        assert_eq!(t3_server::cli_commands::build_pairing_url(row["base"].as_str().unwrap(), row["token"].as_str().unwrap()).unwrap(), row["url"].as_str().unwrap(), "{row}");
        checked += 1;
    }
    for row in fixture_rows("qr") {
        assert_eq!(t3_server::cli_commands::render_terminal_qr_code(row["url"].as_str().unwrap()), row["output"].as_str().unwrap(), "{row}");
        checked += 1;
    }
    for row in fixture_rows("duration") {
        let got = t3_server::cli_commands::parse_duration(row["text"].as_str().unwrap()).map(|duration| duration.num_milliseconds());
        assert_eq!(got, row["millis"].as_i64(), "{row}");
        checked += 1;
    }
    assert!(checked >= 55, "{checked}");
}

// ---------------------------------------------------------------------------------------------
// pair
// ---------------------------------------------------------------------------------------------

#[test]
fn pair_prints_the_credential_alone_and_takes_scopes_so_an_administrative_one_can_be_minted() {
    let home = home();
    let state = home.state();
    let plain = home.run(&["pair", "--state-dir", &state.to_string_lossy()]);
    let credential = plain.ok().trim_end().to_owned();
    assert_eq!((credential.len(), plain.stdout.lines().count()), (12, 1), "{}", plain.stdout);
    let auth = auth_over(&state);
    let (session, _) = auth.exchange_pairing_bearer(&credential, None, json!({}), chrono::Utc::now()).unwrap();
    assert!(session.scopes.len() >= 10, "the default is the broad set pair has always granted: {:?}", session.scopes);

    // An administrative credential, as the startup URL carries.
    let mut args = vec!["pair".to_owned(), "--base-dir".into(), home.base_arg(), "--label".into(), "ops".into(), "--ttl".into(), "30m".into()];
    for scope in t3_contracts::AUTH_ADMINISTRATIVE_SCOPES {
        args.push("--scope".into());
        args.push(serde_json::to_value(scope).unwrap().as_str().unwrap().to_owned());
    }
    let admin = home.run(&args.iter().map(String::as_str).collect::<Vec<_>>());
    let (session, _) = auth.exchange_pairing_bearer(admin.ok().trim_end(), None, json!({}), chrono::Utc::now()).unwrap();
    assert_eq!(session.scopes.len(), t3_contracts::AUTH_ADMINISTRATIVE_SCOPES.len());
    assert_eq!(session.client["label"], "ops");

    let narrow = home.run(&["pair", "--state-dir", &state.to_string_lossy(), "--scope", "orchestration:read"]);
    let (session, _) = auth.exchange_pairing_bearer(narrow.ok().trim_end(), None, json!({}), chrono::Utc::now()).unwrap();
    assert_eq!(session.scopes, vec![t3_contracts::AuthEnvironmentScope::OrchestrationRead]);
    assert_eq!(home.run(&["pair", "--state-dir", &state.to_string_lossy(), "--scope", "everything"]).code, 1);
}

// ---------------------------------------------------------------------------------------------
// A running server
// ---------------------------------------------------------------------------------------------

struct Server {
    child: Child,
    lines: mpsc::Receiver<String>,
    port: u16,
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn settings(home: &Home) -> PathBuf {
    let path = home.root.join("settings.json");
    let mut defaults = serde_json::to_value(t3_contracts::ServerSettings::default()).unwrap();
    for (_, config) in defaults["providers"].as_object_mut().unwrap() {
        config["enabled"] = json!(false);
    }
    std::fs::write(&path, serde_json::to_vec(&defaults).unwrap()).unwrap();
    path
}

/// The directory of a web app (an index and nothing else): the original answers its readiness probe at `/` from the web app it
/// bundles, and this server from `--assets` or the `client` directory beside its executable.
fn assets(home: &Home) -> PathBuf {
    let directory = home.root.join("client");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("index.html"), "<!doctype html><title>T3</title>").unwrap();
    directory
}

/// The server as the remote launch starts it: `serve --host 127.0.0.1 --port N --base-dir <home>`, with `T3CODE_NO_BROWSER=1`.
fn start_as_launched(home: &Home, port: u16) -> Server {
    let mut command = home.command("/usr/bin:/bin");
    command
        .env("T3CODE_NO_BROWSER", "1")
        .env("RUST_LOG", "t3_server=info")
        .args(["serve", "--host", "127.0.0.1", "--port", &port.to_string(), "--base-dir"])
        .arg(home.base())
        .arg("--settings")
        .arg(settings(home))
        .arg("--assets")
        .arg(assets(home))
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn().unwrap();
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
    Server { child, lines, port: ready["port"].as_u64().unwrap() as u16 }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_remote_launch_sequence_works_against_this_binary() {
    let home = home();
    let port = free_port();
    let server = start_as_launched(&home, port);
    assert_eq!(server.port, port);
    let _ = &server.lines;
    let base = home.base_arg();
    // The launch script waits for the port, then finds the default server's runtime record, then pairs.
    assert_eq!(home.run(&["__ssh-helper", "wait-ready", &port.to_string(), "10000", "2000"]).code, 0);
    let runtime = home.state().join("server-runtime.json");
    let found = home.run(&["__ssh-helper", "runtime-port", &runtime.to_string_lossy()]);
    assert_eq!(found.ok(), format!("{} {port}", server.child.id()));
    let issued = home.run(&["auth", "pairing", "create", "--base-dir", &base, "--json"]).json();
    let credential = issued["credential"].as_str().unwrap();
    // The credential works on the running server: it pairs a browser session.
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let response = client.post(format!("http://127.0.0.1:{port}/api/auth/browser-session")).json(&json!({"credential":credential})).send().await.unwrap();
    assert_eq!(response.status(), 200);
    let paired: Value = response.json().await.unwrap();
    assert_eq!(paired["authenticated"], true);
    // And it is single use.
    assert_eq!(client.post(format!("http://127.0.0.1:{port}/api/auth/browser-session")).json(&json!({"credential":credential})).send().await.unwrap().status(), 401);
}

#[tokio::test(flavor = "multi_thread")]
async fn projects_are_added_renamed_and_removed_on_a_running_server_and_on_the_state_alone() {
    let home = home();
    let workspace = home.root.join("workspaces").join("alpha");
    std::fs::create_dir_all(&workspace).unwrap();
    let beta = home.root.join("workspaces").join("beta");
    std::fs::create_dir_all(&beta).unwrap();
    let base = home.base_arg();

    // On the state alone (no server has run here).
    let added = home.run(&["project", "add", "--base-dir", &base, &workspace.to_string_lossy()]);
    let line = added.ok().trim_end().to_owned();
    let id = line.strip_prefix("Added project ").unwrap().split(' ').next().unwrap().to_owned();
    assert_eq!(line, format!("Added project {id} (alpha) at {}.", workspace.display()));
    let again = home.run(&["project", "add", "--base-dir", &base, &workspace.to_string_lossy()]);
    assert_eq!((again.code, again.stderr.as_str()), (1, format!("An active project already exists for '{}'.\n", workspace.display()).as_str()));
    assert_eq!(home.run(&["project", "add", "--base-dir", &base, "--title", "   ", &beta.to_string_lossy()]).stderr, "Project title cannot be empty.\n");
    assert_eq!(home.run(&["project", "add", "--base-dir", &base, &home.arg("workspaces/missing")]).code, 1);
    let titled = home.run(&["project", "add", "--base-dir", &base, "--title", "  Second  ", &beta.to_string_lossy()]);
    assert!(titled.ok().contains(" (Second) at "), "{}", titled.stdout);

    // By id and by workspace root.
    assert_eq!(home.run(&["project", "rename", "--base-dir", &base, &id, "First"]).ok(), format!("Renamed project {id} to First.\n"));
    assert_eq!(home.run(&["project", "rename", "--base-dir", &base, &workspace.to_string_lossy(), "First"]).ok(), format!("Project {id} is already named First.\n"));
    assert_eq!(home.run(&["project", "rename", "--base-dir", &base, &id, "  "]).stderr, "Project title cannot be empty.\n");
    assert_eq!(home.run(&["project", "rename", "--base-dir", &base, "nothing", "X"]).stderr, "No active project found for 'nothing'.\n");
    assert_eq!(home.run(&["project", "remove", "--base-dir", &base, " "]).stderr, "Project identifier cannot be empty.\n");
    assert_eq!(home.run(&["project", "remove", "--base-dir", &base, &beta.to_string_lossy()]).ok(), format!("Removed project {} (Second).\n", titled.stdout.split(' ').nth(2).unwrap()));
    assert_eq!(home.run(&["project", "remove", "--base-dir", &base, &beta.to_string_lossy()]).code, 1);
    // A temporary session the command made for itself is gone.
    assert_eq!(home.run(&["auth", "session", "list", "--base-dir", &base, "--json"]).json().as_array().unwrap().len(), 0);

    // On a running server: the same commands, answered through it. A project it did not know of before shows in its snapshot.
    let port = free_port();
    let server = start_as_launched(&home, port);
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let token = home.run(&["auth", "session", "issue", "--base-dir", &base, "--token-only"]).ok().trim_end().to_owned();
    let snapshot = || async {
        let response = client.get(format!("http://127.0.0.1:{port}/api/projects")).bearer_auth(&token).send().await.unwrap();
        response.json::<Value>().await.unwrap()["projects"].as_array().unwrap().iter().map(|project| project["title"].as_str().unwrap().to_owned()).collect::<Vec<_>>()
    };
    assert_eq!(snapshot().await, ["First"]);
    let gamma = home.root.join("workspaces").join("gamma");
    std::fs::create_dir_all(&gamma).unwrap();
    assert!(home.run(&["project", "add", "--base-dir", &base, &gamma.to_string_lossy()]).ok().contains("(gamma)"));
    assert_eq!(snapshot().await, ["First", "gamma"]);
    assert_eq!(home.run(&["project", "rename", "--base-dir", &base, &gamma.to_string_lossy(), "Third"]).code, 0);
    assert_eq!(snapshot().await, ["First", "Third"]);
    assert_eq!(home.run(&["project", "remove", "--base-dir", &base, "Third"]).stderr, "No active project found for 'Third'.\n");
    assert_eq!(home.run(&["project", "remove", "--base-dir", &base, &gamma.to_string_lossy()]).code, 0);
    assert_eq!(snapshot().await, ["First"]);
    // Only the session this test issued is left: the commands revoked their own.
    assert_eq!(home.run(&["auth", "session", "list", "--base-dir", &base, "--json"]).json().as_array().unwrap().len(), 1);
    drop(server);

    // A record of a server that is gone is cleared, and the command runs on the state.
    let runtime = home.state().join("server-runtime.json");
    std::fs::write(&runtime, json!({"version":1,"pid":1,"host":"127.0.0.1","port":port,"origin":format!("http://127.0.0.1:{port}"),"startedAt":"2026-10-11T12:00:00.000Z"}).to_string()).unwrap();
    let delta = home.root.join("workspaces").join("delta");
    std::fs::create_dir_all(&delta).unwrap();
    assert!(home.run(&["project", "add", "--base-dir", &base, &delta.to_string_lossy()]).ok().contains("(delta)"));
    assert!(!runtime.exists(), "a record nothing answers at is cleared");
}

// ---------------------------------------------------------------------------------------------
// pair --tailscale
// ---------------------------------------------------------------------------------------------

#[test]
fn pair_with_tailscale_names_what_is_missing_without_touching_anything() {
    let home = home();
    let base = home.base_arg();
    // No server has run here.
    let none = home.run(&["pair", "--tailscale", "--base-dir", &base]);
    assert_eq!(none.code, 1);
    assert!(none.stderr.starts_with("No running T3 Code server found.\n  checked ") && none.stderr.contains("server-runtime.json"), "{}", none.stderr);
    assert!(none.stderr.contains("Start one with `npx t3 serve`"), "{}", none.stderr);
    assert_eq!(home.run(&["pair", "--tailscale", "--base-dir", &base, "--tailscale-serve-port", "70000"]).code, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn pair_with_tailscale_asks_the_tailscale_on_the_path_and_stops_where_it_cannot_go_on() {
    let home = home();
    let port = free_port();
    let _server = start_as_launched(&home, port);
    let base = home.base_arg();
    // A `tailscale` at an absolute path of this test's own, first on the PATH: nothing real is run.
    let bin = home.root.join("fakebin");
    std::fs::create_dir_all(&bin).unwrap();
    let log = home.root.join("tailscale.log");
    let write_fake = |status: &str| {
        use std::os::unix::fs::PermissionsExt;
        let path = bin.join("tailscale");
        std::fs::write(&path, format!("#!/bin/sh\necho \"$@\" >> '{}'\nif [ \"$1\" = status ]; then cat <<'JSON'\n{status}\nJSON\nfi\n", log.display())).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    write_fake(r#"{"Self":{"TailscaleIPs":["100.64.0.5"]}}"#);
    let path = format!("{}:/usr/bin:/bin", bin.display());
    let output = home.run_with_path(&path, &["pair", "--tailscale", "--base-dir", &base]);
    assert_eq!(output.code, 1);
    assert_eq!(output.stderr, "This machine has no MagicDNS name. Run `tailscale up` and enable MagicDNS.\n");
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "status --json\n", "nothing was served");
    // Tailscale down is reported as such.
    write_fake("not json");
    let down = home.run_with_path(&path, &["pair", "--tailscale", "--base-dir", &base]);
    assert_eq!(down.stderr, "Could not talk to Tailscale. Is tailscaled running? Try `tailscale status`.\n");
}

struct Answers {
    probes: std::sync::Mutex<Vec<t3_remote::tailscale_pair::EnvironmentProbeResult>>,
    served: std::sync::Mutex<Vec<(u16, u16, Option<String>)>>,
}
#[async_trait::async_trait]
impl t3_remote::tailscale_pair::EnvironmentProber for Answers {
    async fn probe(&self, _: &str) -> t3_remote::tailscale_pair::EnvironmentProbeResult {
        let mut probes = self.probes.lock().unwrap();
        if probes.len() > 1 { probes.remove(0) } else { probes[0].clone() }
    }
}
#[async_trait::async_trait]
impl t3_remote::tailscale_pair::PairTailscale for Answers {
    async fn read_status(&self) -> Result<t3_remote::tailscale::TailscaleStatus, String> {
        Ok(t3_remote::tailscale::TailscaleStatus { magic_dns_name: Some("box.tail.ts.net".into()), tailnet_ipv4_addresses: vec![] })
    }
    async fn ensure_serve(&self, local_port: u16, serve_port: u16, local_host: Option<&str>) -> Result<(), String> {
        self.served.lock().unwrap().push((local_port, serve_port, local_host.map(str::to_owned)));
        Ok(())
    }
}

#[tokio::test(start_paused = true)]
async fn the_tailnet_url_is_published_once_and_the_pairing_output_carries_the_url_the_qr_and_the_notes() {
    use t3_remote::tailscale_pair::EnvironmentProbeResult as Probe;
    let home = home();
    let state = home.state();
    std::fs::create_dir_all(&state).unwrap();
    // A record of a live process (this one) whose origin answers with this environment.
    let descriptor: t3_contracts::ExecutionEnvironmentDescriptor = serde_json::from_value(json!({"environmentId":"env-1","label":"My box","platform":{"os":"linux","arch":"x64"},"serverVersion":"1.0.0","capabilities":{"repositoryIdentity":false,"connectionProbe":true}})).unwrap();
    std::fs::write(state.join("server-runtime.json"), json!({"version":1,"pid":std::process::id(),"host":"127.0.0.1","port":3773,"origin":"http://127.0.0.1:3773","startedAt":"2026-10-11T12:00:00.000Z"}).to_string()).unwrap();
    // Nothing answers on the tailnet URL at first, and then the environment does.
    let answers = Answers { probes: std::sync::Mutex::new(vec![Probe::Descriptor(Box::new(descriptor.clone())), Probe::Unreachable, Probe::Descriptor(Box::new(descriptor.clone()))]), served: Default::default() };
    let (target, base) = t3_server::cli_commands::tailscale_pairing_base(&answers, &answers, &state, 443).await.unwrap();
    assert_eq!((target.label.as_str(), target.origin.as_str(), target.environment_id.as_str()), ("My box", "http://127.0.0.1:3773", "env-1"));
    assert_eq!(base.base_url, "https://box.tail.ts.net/");
    assert_eq!(*answers.served.lock().unwrap(), vec![(3773, 443, None)]);
    assert_eq!(base.notes, ["Tailscale Serve now maps https://box.tail.ts.net/ to this server and persists across restarts. Remove it with `tailscale serve --https=443 off`."]);

    let url = t3_server::cli_commands::build_pairing_url(&base.base_url, "ABCD2345EFGH").unwrap();
    assert_eq!(url, "https://box.tail.ts.net/pair#token=ABCD2345EFGH");
    let output = t3_server::cli_commands::format_pair_output(&target.label, &target.origin, &url, "ABCD2345EFGH", chrono::Utc::now(), &base.notes);
    let lines: Vec<&str> = output.lines().collect();
    assert_eq!(lines[0], "Pairing with My box (http://127.0.0.1:3773).");
    assert!(output.contains(&t3_server::cli_commands::render_terminal_qr_code(&url)));
    assert!(output.contains("\nPairing URL: https://box.tail.ts.net/pair#token=ABCD2345EFGH\nToken: ABCD2345EFGH\nExpires: 20"));
    assert!(output.contains("\n\nNote: Tailscale Serve now maps ") && output.ends_with("\n"));

    // A mapping that already fronts another environment is not replaced.
    let mut other = descriptor.clone();
    other.environment_id = "env-2".parse().unwrap();
    let occupied = Answers { probes: std::sync::Mutex::new(vec![Probe::Descriptor(Box::new(other))]), served: Default::default() };
    // The first probe is of the local origin and must be this environment; build the answers in that order.
    occupied.probes.lock().unwrap().insert(0, Probe::Descriptor(Box::new(descriptor.clone())));
    let error = t3_server::cli_commands::tailscale_pairing_base(&occupied, &occupied, &state, 8443).await.unwrap_err();
    assert_eq!(error.to_string(), "Tailscale Serve on HTTPS port 8443 already fronts a different T3 Code server. Pass --tailscale-serve-port to publish this one on another port.");
    assert!(occupied.served.lock().unwrap().is_empty());
    // A record whose process is gone, or whose origin is not a T3 server, is no server.
    std::fs::write(state.join("server-runtime.json"), json!({"version":1,"pid":999_999_999,"port":3773,"origin":"http://127.0.0.1:3773","startedAt":"x"}).to_string()).unwrap();
    assert!(matches!(t3_server::cli_commands::tailscale_pairing_base(&answers, &answers, &state, 443).await.unwrap_err(), t3_remote::tailscale_pair::PairError::NoRunningServer { .. }));
}
