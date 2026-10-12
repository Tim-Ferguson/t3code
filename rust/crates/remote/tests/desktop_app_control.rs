//! The desktop control socket: the address and the protocol lines against the original's own answers
//! (`tests/fixtures/desktop-app-control.json`, written by `rust/tools/generate_desktop_app_control_fixtures.mjs`), and the
//! client and server halves over a real Unix socket in a temporary directory, with the ownership checks of the original.
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use t3_contracts::{DesktopAppActivationErrorCode, DesktopAppActivationRequest, DesktopAppActivationResponse};
use t3_remote::desktop_app_control::{ControlAddress, ControlHandler, SendError, resolve_address, send_request, start_control_server};

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/desktop-app-control.json")).unwrap()
}

#[test]
fn the_address_is_the_one_the_original_resolves() {
    let rows = fixture()["addresses"].as_array().unwrap().clone();
    assert_eq!(rows.len(), 72);
    for row in rows {
        let found = resolve_address(Path::new(row["stateDir"].as_str().unwrap()), row["platform"].as_str().unwrap(), Path::new(row["tempDir"].as_str().unwrap()), row["userId"].as_u64().map(|id| id as u32));
        assert_eq!(json!(found.address), row["address"], "{row}");
        assert_eq!(found.directory.map(|directory| directory.to_string_lossy().into_owned()).map_or(Value::Null, Value::from), row["directory"], "{row}");
    }
}

#[test]
fn a_line_is_valid_exactly_when_the_originals_schema_accepts_it() {
    let rows = fixture()["protocol"].as_array().unwrap().clone();
    assert_eq!(rows.len(), 30);
    for row in rows {
        let line = row["value"].to_string();
        let accepted = match row["kind"].as_str().unwrap() {
            "request" => DesktopAppActivationRequest::parse(&line).is_ok(),
            _ => DesktopAppActivationResponse::parse(&line).is_ok(),
        };
        assert_eq!(accepted, row["valid"], "{row}");
    }
}

/// A desktop that answers each request with `reply`, remembers what it was asked, and notes what it was told to cancel.
struct Desktop {
    received: Mutex<Vec<DesktopAppActivationRequest>>,
    cancelled: Mutex<Vec<String>>,
    reply: Box<dyn Fn(&DesktopAppActivationRequest) -> DesktopAppActivationResponse + Send + Sync>,
    /// Held requests wait here until released; the reply is not sent while it is closed.
    gate: Option<tokio::sync::watch::Receiver<bool>>,
    cancel_seen: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl ControlHandler for Desktop {
    async fn handle(&self, request: DesktopAppActivationRequest) -> DesktopAppActivationResponse {
        self.received.lock().unwrap().push(request.clone());
        if let Some(gate) = &self.gate {
            let mut gate = gate.clone();
            let _ = gate.wait_for(|open| *open).await;
        }
        (self.reply)(&request)
    }
    fn cancel(&self, request_id: &str) {
        self.cancelled.lock().unwrap().push(request_id.to_owned());
        self.cancel_seen.notify_one();
    }
}

fn desktop(reply: impl Fn(&DesktopAppActivationRequest) -> DesktopAppActivationResponse + Send + Sync + 'static) -> Arc<Desktop> {
    Arc::new(Desktop { received: Default::default(), cancelled: Default::default(), reply: Box::new(reply), gate: None, cancel_seen: Default::default() })
}

fn opened(request: &DesktopAppActivationRequest) -> DesktopAppActivationResponse {
    DesktopAppActivationResponse::success(request.request_id.as_str(), "project-1", "thread-1").unwrap()
}

fn request(id: &str, root: &str) -> DesktopAppActivationRequest {
    DesktopAppActivationRequest::parse(&json!({"version":1,"requestId":id,"type":"open-workspace","workspaceRoot":root,"platform":"linux"}).to_string()).unwrap()
}

/// A short temporary directory (a Unix socket path may be at most about a hundred bytes) and the address in it.
fn home(name: &str) -> (tempfile::TempDir, ControlAddress) {
    let temp = tempfile::Builder::new().prefix("t3c").tempdir().unwrap();
    let address = resolve_address(Path::new(&format!("/state/{name}")), "linux", temp.path(), t3_remote::desktop_app_control::current_user_id());
    (temp, address)
}

const WAIT: Duration = Duration::from_secs(10);

#[tokio::test]
async fn a_request_reaches_the_desktop_and_its_answer_comes_back() {
    let (_temp, address) = home("answers");
    let app = desktop(opened);
    let server = start_control_server(&address, t3_remote::desktop_app_control::current_user_id(), app.clone()).await.unwrap();
    let asked = request("req-1", "/work/project");
    let answer = send_request(&address.address, None, &asked, WAIT).await.unwrap();
    assert_eq!(answer, DesktopAppActivationResponse::success("req-1", "project-1", "thread-1").unwrap());
    assert_eq!(app.received.lock().unwrap().as_slice(), [asked]);
    server.close().await.unwrap();
    assert!(!Path::new(&address.address).exists(), "closing removes the socket");
}

#[tokio::test]
async fn the_socket_directory_is_checked_and_closed_to_other_users() {
    use std::os::unix::fs::PermissionsExt;
    let user = t3_remote::desktop_app_control::current_user_id();
    // A directory made too open is closed again.
    let (_temp, address) = home("modes");
    let directory = address.directory.clone().unwrap();
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o777)).unwrap();
    let server = start_control_server(&address, user, desktop(opened)).await.unwrap();
    assert_eq!(std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(std::fs::metadata(&address.address).unwrap().permissions().mode() & 0o777, 0o600);
    server.close().await.unwrap();

    // A directory owned by someone else is refused, and so is one that is a link to somewhere else.
    let (_temp, address) = home("owner");
    let someone_else = user.map(|id| id + 1);
    let refused = start_control_server(&address, someone_else, desktop(opened)).await.err().expect("another user's directory is refused");
    assert!(refused.to_string().contains("is owned by another user"), "{refused}");
    let (temp, address) = home("link");
    let elsewhere = temp.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, address.directory.as_ref().unwrap()).unwrap();
    let refused = start_control_server(&address, user, desktop(opened)).await.err().expect("a linked directory is refused");
    assert!(refused.to_string().contains("is not a directory"), "{refused}");
    assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0, "nothing was bound through the link");
}

#[tokio::test]
async fn the_newest_app_takes_the_address_and_closing_never_removes_anothers_socket() {
    let user = t3_remote::desktop_app_control::current_user_id();
    let (_temp, address) = home("takeover");
    let first = desktop(|request| DesktopAppActivationResponse::success(request.request_id.as_str(), "first", "t").unwrap());
    let second = desktop(|request| DesktopAppActivationResponse::success(request.request_id.as_str(), "second", "t").unwrap());
    let older = start_control_server(&address, user, first).await.unwrap();
    let newer = start_control_server(&address, user, second).await.unwrap();
    let answer = send_request(&address.address, None, &request("r", "/w"), WAIT).await.unwrap();
    assert!(matches!(&answer, DesktopAppActivationResponse::Success(found) if found.project_id.as_str() == "second"), "the newest app answers");
    // The older app quits: its close leaves the newer app's socket where it is.
    older.close().await.unwrap();
    let answer = send_request(&address.address, None, &request("r2", "/w"), WAIT).await.unwrap();
    assert!(matches!(&answer, DesktopAppActivationResponse::Success(found) if found.project_id.as_str() == "second"));
    // Reclaiming never replaces a socket that exists.
    newer.reclaim().await.unwrap();
    // The socket file is gone (another cleanup removed it): the app binds the address again; an app that already did so is not displaced.
    std::fs::remove_file(&address.address).unwrap();
    newer.reclaim().await.unwrap();
    let answer = send_request(&address.address, None, &request("r3", "/w"), WAIT).await.unwrap();
    assert!(matches!(&answer, DesktopAppActivationResponse::Success(found) if found.project_id.as_str() == "second"));
    newer.close().await.unwrap();
    assert!(!Path::new(&address.address).exists());
}

#[tokio::test]
async fn a_malformed_line_is_answered_invalid_with_the_id_it_carried() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (_temp, address) = home("malformed");
    let app = desktop(opened);
    let server = start_control_server(&address, t3_remote::desktop_app_control::current_user_id(), app.clone()).await.unwrap();
    let ask = |line: String| {
        let path = address.address.clone();
        async move {
            let mut stream = tokio::net::UnixStream::connect(path).await.unwrap();
            stream.write_all(line.as_bytes()).await.unwrap();
            let mut answer = String::new();
            stream.read_to_string(&mut answer).await.unwrap();
            serde_json::from_str::<Value>(answer.trim()).unwrap()
        }
    };
    let not_json = ask("{nope\n".into()).await;
    assert_eq!((not_json["requestId"].as_str(), not_json["code"].as_str(), not_json["message"].as_str()), (Some("invalid-request"), Some("invalid-request"), Some("The desktop app request is not valid JSON.")));
    let wrong = ask(format!("{}\n", json!({"version":1,"requestId":"mine","type":"open-workspace"}))).await;
    assert_eq!((wrong["requestId"].as_str(), wrong["code"].as_str(), wrong["message"].as_str()), (Some("mine"), Some("invalid-request"), Some("The desktop app request is invalid.")));
    let anonymous = ask(format!("{}\n", json!({"requestId":"  "}))).await;
    assert_eq!(anonymous["requestId"], "invalid-request");
    let too_big = ask(format!("{}\n", "x".repeat(70 * 1024))).await;
    assert_eq!(too_big["message"], "The desktop app request is too large.");
    assert!(app.received.lock().unwrap().is_empty(), "nothing malformed reached the shell");
    server.close().await.unwrap();
}

#[tokio::test]
async fn a_cli_that_goes_away_cancels_its_request() {
    use tokio::io::AsyncWriteExt;
    let (_temp, address) = home("cancel");
    let (_release, gate) = tokio::sync::watch::channel(false);
    let app = Arc::new(Desktop { received: Default::default(), cancelled: Default::default(), reply: Box::new(opened), gate: Some(gate), cancel_seen: Default::default() });
    let server = start_control_server(&address, t3_remote::desktop_app_control::current_user_id(), app.clone()).await.unwrap();
    let mut stream = tokio::net::UnixStream::connect(&address.address).await.unwrap();
    stream.write_all(format!("{}\n", serde_json::to_string(&request("held", "/w")).unwrap()).as_bytes()).await.unwrap();
    drop(stream);
    tokio::time::timeout(WAIT, app.cancel_seen.notified()).await.expect("the shell is told");
    assert_eq!(app.cancelled.lock().unwrap().as_slice(), ["held"]);
    server.close().await.unwrap();
}

/// A one-shot peer that answers whatever it is sent with `line` (or closes, for none), to see what the client makes of it.
async fn peer(directory: &Path, line: Option<String>) -> String {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let path = directory.join("p.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut asked = String::new();
        tokio::io::BufReader::new(reader).read_line(&mut asked).await.unwrap();
        if let Some(line) = line {
            writer.write_all(line.as_bytes()).await.unwrap();
            writer.shutdown().await.unwrap();
        }
    });
    path.to_string_lossy().into_owned()
}

#[tokio::test]
async fn the_client_accepts_only_a_valid_answer_to_its_own_request() {
    let asked = request("mine", "/w");
    let dir = tempfile::Builder::new().prefix("t3c").tempdir().unwrap();
    let message = |result: Result<DesktopAppActivationResponse, SendError>| result.err().expect("refused").message;
    let cases: Vec<(Option<String>, &str)> = vec![
        (Some("not json\n".into()), "The desktop app response is not valid JSON."),
        (Some(format!("{}\n", json!({"invalid":true}))), "The desktop app response is invalid."),
        (Some(format!("{}\n", json!({"version":1,"requestId":"theirs","ok":true,"projectId":"p","threadId":"t"}))), "The desktop app response did not match this request."),
        (Some(format!("{}\n", "x".repeat(70 * 1024))), "The desktop app response is too large."),
        (None, "The desktop app closed the connection."),
    ];
    for (index, (line, expected)) in cases.into_iter().enumerate() {
        let sub = dir.path().join(index.to_string());
        std::fs::create_dir(&sub).unwrap();
        let address = peer(&sub, line).await;
        assert_eq!(message(send_request(&address, None, &asked, WAIT).await), expected);
    }
}

#[tokio::test]
async fn an_unreachable_address_falls_back_once_and_a_failure_answer_never_does() {
    let dir = tempfile::Builder::new().prefix("t3c").tempdir().unwrap();
    let missing = dir.path().join("missing.sock").to_string_lossy().into_owned();
    let asked = request("fallback", "/w");
    // Nothing at either address: the failure is the first address's, with its code.
    let error = send_request(&missing, None, &asked, WAIT).await.err().unwrap();
    assert_eq!(error.code.as_deref(), Some("ENOENT"));
    let other_missing = dir.path().join("other.sock").to_string_lossy().into_owned();
    let error = send_request(&missing, Some(&other_missing), &asked, WAIT).await.err().unwrap();
    assert_eq!(error.code.as_deref(), Some("ENOENT"), "the fallback is tried once and its error is the one reported");
    // A socket file with nothing listening is refused, which also falls back.
    let stale = dir.path().join("stale.sock");
    drop(std::os::unix::net::UnixListener::bind(&stale).unwrap());
    let (_temp, address) = home("fallback");
    let app = desktop(opened);
    let server = start_control_server(&address, t3_remote::desktop_app_control::current_user_id(), app.clone()).await.unwrap();
    let answer = send_request(stale.to_str().unwrap(), Some(&address.address), &asked, WAIT).await.unwrap();
    assert!(matches!(answer, DesktopAppActivationResponse::Success(_)));
    assert_eq!(app.received.lock().unwrap().len(), 1);
    server.close().await.unwrap();

    // Once connected there is no fallback: a failure answer is the answer, and the other desktop is never asked.
    let (_temp, first) = home("first");
    let (_other, second) = home("second");
    let refusing = desktop(|request| DesktopAppActivationResponse::failure(request.request_id.as_str(), DesktopAppActivationErrorCode::ProjectCreateFailed, "The project path is not available."));
    let fallback = desktop(opened);
    let user = t3_remote::desktop_app_control::current_user_id();
    let first_server = start_control_server(&first, user, refusing.clone()).await.unwrap();
    let second_server = start_control_server(&second, user, fallback.clone()).await.unwrap();
    let answer = send_request(&first.address, Some(&second.address), &asked, WAIT).await.unwrap();
    assert!(matches!(&answer, DesktopAppActivationResponse::Failure(found) if found.code == DesktopAppActivationErrorCode::ProjectCreateFailed));
    assert!(fallback.received.lock().unwrap().is_empty());
    first_server.close().await.unwrap();
    second_server.close().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_desktop_that_never_answers_is_given_up_on_at_the_deadline() {
    let (_temp, address) = home("slow");
    let (_release, gate) = tokio::sync::watch::channel(false);
    let app = Arc::new(Desktop { received: Default::default(), cancelled: Default::default(), reply: Box::new(opened), gate: Some(gate), cancel_seen: Default::default() });
    let server = start_control_server(&address, t3_remote::desktop_app_control::current_user_id(), app.clone()).await.unwrap();
    let error = send_request(&address.address, None, &request("slow", "/w"), t3_remote::desktop_app_control::CLI_RESPONSE_TIMEOUT).await.err().unwrap();
    assert_eq!(error.message, "The desktop app did not respond in time.");
    assert_eq!(app.received.lock().unwrap().len(), 1, "the request did arrive");
    server.close().await.unwrap();
}

#[test]
fn the_deadlines_are_the_originals() {
    use t3_remote::desktop_app_control::*;
    assert_eq!((CLI_RESPONSE_TIMEOUT.as_millis(), DESKTOP_REQUEST_TIMEOUT.as_millis(), CONNECTION_IDLE_TIMEOUT.as_millis(), MAX_LINE_BYTES), (17_000, 15_000, 5_000, 65_536));
}
