//! `t3 app` against a stand-in for the desktop app's control socket (`cli/app.test.ts` of the original): the checks before it asks,
//! which home it asks at, when it falls back to a development desktop, and what it prints.
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use t3_contracts::{DesktopAppActivationErrorCode, DesktopAppActivationRequest, DesktopAppActivationResponse};
use t3_remote::desktop_app_control::{ControlHandler, ControlServer, current_user_id, resolve_address, start_control_server};
use t3_server::cli_app::{Host, SSH_UNSUPPORTED, UNREACHABLE, app_command_on};

type Reply = Box<dyn Fn(&DesktopAppActivationRequest) -> String + Send + Sync>;

struct FakeDesktop {
    received: Mutex<Vec<DesktopAppActivationRequest>>,
    reply: Reply,
}

#[async_trait::async_trait]
impl ControlHandler for FakeDesktop {
    async fn handle(&self, request: DesktopAppActivationRequest) -> DesktopAppActivationResponse {
        self.received.lock().unwrap().push(request.clone());
        // The reply is a line, so a test can answer with something that is not a response at all.
        let line = (self.reply)(&request);
        DesktopAppActivationResponse::parse(&line).unwrap_or_else(|_| DesktopAppActivationResponse::failure(request.request_id.as_str(), DesktopAppActivationErrorCode::InternalError, "unparsed"))
    }
    fn cancel(&self, _request_id: &str) {}
}

struct World {
    runtime: tokio::runtime::Runtime,
    temp: tempfile::TempDir,
    servers: Vec<ControlServer>,
}

impl World {
    fn new() -> Self {
        Self { runtime: tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap(), temp: tempfile::Builder::new().prefix("t3c").tempdir().unwrap(), servers: Vec::new() }
    }

    /// A desktop listening for the home `base` with the state subdirectory named.
    fn desktop(&mut self, base: &Path, subdirectory: &str, reply: impl Fn(&DesktopAppActivationRequest) -> String + Send + Sync + 'static) -> Arc<FakeDesktop> {
        let app = Arc::new(FakeDesktop { received: Default::default(), reply: Box::new(reply) });
        let address = resolve_address(&base.join(subdirectory), "linux", self.temp.path(), current_user_id());
        let server = self.runtime.block_on(start_control_server(&address, current_user_id(), app.clone())).unwrap();
        self.servers.push(server);
        app
    }

    fn run(&self, args: &[&str], env: &[(&str, &str)], home: &Path) -> t3_server::cli_commands::Outcome {
        let env = env.iter().map(|(key, value)| ((*key).to_owned(), (*value).to_owned())).collect::<Vec<_>>();
        let lookup = |name: &str| if name == "HOME" { Some(home.to_string_lossy().into_owned()) } else { env.iter().find(|(key, _)| key == name).map(|(_, value)| value.clone()) };
        let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        app_command_on(&args, Host { env: &lookup, platform: "linux", cwd: PathBuf::from("/work/current"), temp_dir: self.temp.path().to_path_buf(), user_id: current_user_id(), request_id: "request-1".into() })
    }
}

impl Drop for World {
    fn drop(&mut self) {
        for server in &self.servers {
            let _ = self.runtime.block_on(server.close());
        }
    }
}

fn opened(request: &DesktopAppActivationRequest) -> String {
    json!({"version":1,"requestId":request.request_id.as_str(),"ok":true,"projectId":"project-1","threadId":"thread-1"}).to_string()
}

#[test]
fn ssh_and_unsupported_platforms_are_refused_before_anything_is_looked_for() {
    let world = World::new();
    let home = world.temp.path().join("t3-home");
    let missing = home.to_string_lossy().into_owned();
    for variable in ["SSH_CONNECTION", "SSH_TTY"] {
        let refused = world.run(&["--base-dir", &missing], &[(variable, "client server")], world.temp.path());
        assert_eq!((refused.code, refused.stderr.as_str(), refused.stdout.as_str()), (1, format!("{SSH_UNSUPPORTED}\n").as_str(), ""));
    }
    // An empty variable is no variable.
    let unreachable = world.run(&["--base-dir", &missing], &[("SSH_CONNECTION", "")], world.temp.path());
    assert_eq!(unreachable.stderr, format!("{UNREACHABLE}\n"));
    let args = vec!["--base-dir".to_owned(), missing.clone()];
    let freebsd = app_command_on(&args, Host { env: &|_| None, platform: "freebsd", cwd: PathBuf::from("/"), temp_dir: world.temp.path().to_path_buf(), user_id: None, request_id: "r".into() });
    assert_eq!((freebsd.code, freebsd.stderr.as_str()), (1, "`t3 app` is not supported on freebsd.\n"));
    assert!(!home.exists(), "no state is created");
}

#[test]
fn nothing_is_created_when_only_a_server_or_no_desktop_is_running() {
    let world = World::new();
    let home = world.temp.path().join("missing-t3-home");
    let outcome = world.run(&["--base-dir", home.to_str().unwrap()], &[], world.temp.path());
    assert_eq!((outcome.code, outcome.stderr.as_str()), (1, format!("{UNREACHABLE}\n").as_str()));
    assert!(!home.exists());
}

#[test]
fn the_home_comes_from_the_flag_or_the_environment_and_the_project_from_the_path_or_the_working_directory() {
    let mut world = World::new();
    let home = world.temp.path().join("t3-home");
    let app = world.desktop(&home, "userdata", opened);
    let current = world.run(&[], &[("T3CODE_HOME", home.to_str().unwrap())], world.temp.path());
    assert_eq!((current.code, current.stdout.as_str()), (0, "Opened /work/current in T3 Code.\n"));
    let explicit = world.run(&["/work/other/../project", "--base-dir", home.to_str().unwrap()], &[], world.temp.path());
    assert_eq!((explicit.code, explicit.stdout.as_str()), (0, "Opened /work/project in T3 Code.\n"));
    let relative = world.run(&["nested/dir", "--base-dir", home.to_str().unwrap()], &[], world.temp.path());
    assert_eq!(relative.stdout, "Opened /work/current/nested/dir in T3 Code.\n");
    let tilde = world.run(&["~/code", "--base-dir", home.to_str().unwrap()], &[], Path::new("/Users/me"));
    assert_eq!(tilde.stdout, "Opened /Users/me/code in T3 Code.\n");
    let received = app.received.lock().unwrap();
    assert_eq!(received.iter().map(|request| request.workspace_root.as_str()).collect::<Vec<_>>(), ["/work/current", "/work/project", "/work/current/nested/dir", "/Users/me/code"]);
    assert!(received.iter().all(|request| request.platform == t3_contracts::DesktopAppActivationPlatform::Linux && request.request_id.as_str() == "request-1"));
}

#[test]
fn the_installed_desktop_wins_over_a_development_one_and_the_development_one_is_found_when_it_is_alone() {
    let mut world = World::new();
    let home = world.temp.path().join("home");
    let t3 = home.join(".t3");
    let installed = world.desktop(&t3, "userdata", opened);
    let development = world.desktop(&t3, "dev", opened);
    let both = world.run(&[], &[], &home);
    assert_eq!(both.code, 0, "{}", both.stderr);
    assert_eq!((installed.received.lock().unwrap().len(), development.received.lock().unwrap().len()), (1, 0));

    // Alone, the development desktop answers, for no variable and for a blank one.
    let mut world = World::new();
    let home = world.temp.path().join("home");
    let development = world.desktop(&home.join(".t3"), "dev", opened);
    assert_eq!(world.run(&[], &[], &home).code, 0);
    assert_eq!(world.run(&[], &[("T3CODE_HOME", "   ")], &home).code, 0);
    assert_eq!(development.received.lock().unwrap().len(), 2);
    assert!(!home.join(".t3").exists(), "the home is only named, never made");
}

#[test]
fn a_development_desktop_is_never_searched_for_an_explicit_home() {
    let mut world = World::new();
    let home = world.temp.path().join("home");
    let t3 = home.join(".t3");
    let development = world.desktop(&t3, "dev", opened);
    let by_flag = world.run(&["--base-dir", t3.to_str().unwrap()], &[], &home);
    let by_variable = world.run(&[], &[("T3CODE_HOME", t3.to_str().unwrap())], &home);
    assert_eq!((by_flag.stderr.as_str(), by_variable.stderr.as_str()), (format!("{UNREACHABLE}\n").as_str(), format!("{UNREACHABLE}\n").as_str()));
    assert!(development.received.lock().unwrap().is_empty());
}

#[test]
fn a_failure_answer_from_the_default_desktop_is_final() {
    let mut world = World::new();
    let home = world.temp.path().join("home");
    let t3 = home.join(".t3");
    let desktop = world.desktop(&t3, "userdata", |request| json!({"version":1,"requestId":request.request_id.as_str(),"ok":false,"code":"project-create-failed","message":"The project path is not available."}).to_string());
    let development = world.desktop(&t3, "dev", opened);
    let outcome = world.run(&[], &[], &home);
    assert_eq!((outcome.code, outcome.stderr.as_str()), (1, "T3 Code could not open /work/current (project-create-failed).\n"));
    assert_eq!((desktop.received.lock().unwrap().len(), development.received.lock().unwrap().len()), (1, 0));
}

#[test]
fn a_reply_that_is_no_response_is_final_too_and_reads_as_unreachable() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let mut world = World::new();
    let home = world.temp.path().join("home");
    let t3 = home.join(".t3");
    let development = world.desktop(&t3, "dev", opened);
    // A listener that answers whatever it is asked with an object that is neither a success nor a failure.
    let address = resolve_address(&t3.join("userdata"), "linux", world.temp.path(), current_user_id());
    std::fs::create_dir_all(address.directory.as_ref().unwrap()).unwrap();
    let listener = {
        let _entered = world.runtime.enter();
        tokio::net::UnixListener::bind(&address.address).unwrap()
    };
    world.runtime.spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (reader, mut writer) = stream.into_split();
        tokio::io::BufReader::new(reader).read_line(&mut String::new()).await.unwrap();
        writer.write_all(b"{\"invalid\":true}\n").await.unwrap();
        writer.shutdown().await.unwrap();
    });
    let outcome = world.run(&[], &[], &home);
    assert_eq!((outcome.code, outcome.stderr.as_str()), (1, format!("{UNREACHABLE}\n").as_str()));
    assert!(development.received.lock().unwrap().is_empty(), "no fallback once connected");
}
