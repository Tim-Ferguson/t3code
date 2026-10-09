//! The source control settings and the clone step of adding a project through the mounted UI and
//! the real native server. The server asks its host for tools and APIs through one process-wide
//! `Host`; `world()` puts a private one in place before any server in this test binary starts, so
//! that no real `gh`, `glab`, `az` or network is ever reached: a `PATH` of only `git` and `cat`
//! plus the fake CLIs a test writes, and the GitHub and Bitbucket APIs pointed at a stub on
//! loopback. (The server crate's own fakes live in its integration tests, which this crate cannot
//! import, so this is the minimum equivalent.) Tests that rewrite the fakes run one at a time.
use super::diff_tests::{Fixture, Harness, git, rendered_text, seed, until, wait_for_rendered_text};
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::keys_tests::{PALETTE, key_on, open_palette};
use crate::runtime::transport_tests::{change_control, click_control, control, control_attribute, input_control};
use std::{
    io::{BufRead, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};
use t3_server::source_control_host::{GitHubApiResolver, Host, HostEnv, HostOptions};

type Routes = Arc<Mutex<Vec<(String, u16, String)>>>;
type Seen = Arc<Mutex<Vec<(String, String, Option<String>)>>>;

/// A request the stub received, with what a scripted host needs to answer it by what it asks.
#[derive(Debug, Clone)]
pub(crate) struct Asked {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) query: Option<String>,
    pub(crate) accept: Option<String>,
    pub(crate) body: Option<Value>,
}
/// A scripted answer: status, then body. `None` leaves the request to the canned routes.
pub(crate) type Script = Arc<dyn Fn(&Asked) -> Option<(u16, String)> + Send + Sync>;
type Log = Arc<Mutex<Vec<Asked>>>;

/// A server of canned JSON on loopback, run on a thread of its own so it outlives any one test's
/// runtime. Replies are chosen by the longest route prefix of the request path; the rest are 404.
struct Stub {
    port: u16,
    routes: Routes,
    seen: Seen,
    script: Arc<Mutex<Option<Script>>>,
    log: Log,
}
impl Stub {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (routes, seen): (Routes, Seen) = Default::default();
        let (script, log): (Arc<Mutex<Option<Script>>>, Log) = Default::default();
        let (thread_routes, thread_seen, thread_script, thread_log) = (routes.clone(), seen.clone(), script.clone(), log.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (routes, seen, script, log) = (thread_routes.clone(), thread_seen.clone(), thread_script.clone(), thread_log.clone());
                std::thread::spawn(move || Self::answer(stream, routes, seen, script, log));
            }
        });
        Self { port, routes, seen, script, log }
    }
    fn answer(stream: std::net::TcpStream, routes: Routes, seen: Seen, script: Arc<Mutex<Option<Script>>>, log: Log) {
        let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
        let mut first = String::new();
        if reader.read_line(&mut first).unwrap_or(0) == 0 {
            return;
        }
        let (mut length, mut authorization, mut accept) = (0usize, None, None);
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                break;
            }
            let lower = line.to_lowercase();
            if let Some(value) = lower.strip_prefix("content-length:") {
                length = value.trim().parse().unwrap_or(0);
            }
            if lower.starts_with("accept:") {
                accept = line.split_once(':').map(|(_, value)| value.trim().to_owned());
            }
            if lower.starts_with("authorization:") {
                authorization = Some(line.split_once(':').map(|(_, value)| value.trim().to_owned()).unwrap_or_default());
            }
        }
        let mut body = vec![0; length];
        let _ = reader.read_exact(&mut body);
        let mut parts = first.split_whitespace();
        let (method, path) = (parts.next().unwrap_or_default().to_owned(), parts.next().unwrap_or_default().split('?').next().unwrap_or_default().to_owned());
        let query = first.split_whitespace().nth(1).and_then(|target| target.split_once('?')).map(|(_, query)| query.to_owned());
        let asked = Asked { method: method.clone(), path: path.clone(), query, accept, body: serde_json::from_slice(&body).ok() };
        seen.lock().unwrap().push((method, path.clone(), authorization));
        log.lock().unwrap().push(asked.clone());
        let scripted = script.lock().unwrap().clone().and_then(|script| script(&asked));
        let (status, reply) = scripted.unwrap_or_else(|| routes
            .lock()
            .unwrap()
            .iter()
            .filter(|(prefix, ..)| path.starts_with(prefix.as_str()))
            .max_by_key(|(prefix, ..)| prefix.len())
            .map_or((404, "{}".to_owned()), |(_, status, body)| (*status, body.clone())));
        let mut stream = stream;
        let _ = write!(stream, "HTTP/1.1 {status} Fixture\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}", reply.len());
    }
    fn route(&self, prefix: &str, status: u16, body: Value) {
        let mut routes = self.routes.lock().unwrap();
        routes.retain(|(existing, ..)| existing != prefix);
        routes.push((prefix.to_owned(), status, body.to_string()));
    }
    fn reset(&self) {
        self.routes.lock().unwrap().clear();
        self.seen.lock().unwrap().clear();
        self.log.lock().unwrap().clear();
        *self.script.lock().unwrap() = None;
    }
}

pub(crate) struct World {
    bin: PathBuf,
    stub: Stub,
    /// Tests that rewrite the fake tools take this for their whole run.
    exclusive: tokio::sync::Mutex<()>,
}

/// Puts the process's host in place. Every server of this binary starts through
/// `start_fixture`, which calls this first, so nothing can have used the real host before it.
pub(crate) fn world() -> &'static World {
    static WORLD: OnceLock<World> = OnceLock::new();
    WORLD.get_or_init(|| {
        let directory = tempfile::tempdir().unwrap();
        let bin = std::fs::canonicalize(directory.path()).unwrap();
        // The fakes stay for the life of the process, which is what a tool on a PATH is.
        std::mem::forget(directory);
        for tool in ["git", "cat"] {
            let found = String::from_utf8(std::process::Command::new("which").arg(tool).output().unwrap().stdout).unwrap();
            std::os::unix::fs::symlink(found.trim(), bin.join(tool)).unwrap();
        }
        let stub = Stub::start();
        let port = stub.port;
        let bitbucket = format!("http://127.0.0.1:{port}/bitbucket");
        let resolver: GitHubApiResolver = Arc::new(move |host| (format!("http://127.0.0.1:{port}/rest/{host}"), format!("http://127.0.0.1:{port}/graphql/{host}")));
        // An empty environment: no token variable, no home directory of the developer.
        let host = Host::new(HostOptions {
            env: HostEnv::from_pairs([("T3CODE_BITBUCKET_API_BASE_URL", bitbucket.as_str())]),
            cwd: bin.clone(),
            path: Some(bin.to_string_lossy().into_owned()),
            github_api: Some(resolver),
            ..HostOptions::default()
        });
        assert!(Host::install(host), "the fake host is installed before any server of this binary uses the real one");
        World { bin, stub, exclusive: tokio::sync::Mutex::new(()) }
    })
}

impl World {
    /// Answers every request by what it asks, before the canned routes; ended by the next `reset`.
    pub(crate) fn script(&self, script: Script) {
        *self.stub.script.lock().unwrap() = Some(script);
    }
    /// Every request since the last reset, in order.
    pub(crate) fn asked(&self) -> Vec<Asked> {
        self.stub.log.lock().unwrap().clone()
    }
    pub(crate) fn exclusive(&self) -> &tokio::sync::Mutex<()> {
        &self.exclusive
    }
    pub(crate) fn reset_everything_signed_in(&self) {
        self.everything_signed_in();
    }
    fn install(&self, name: &str, body: &str) {
        let path = self.bin.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    fn remove(&self, name: &str) {
        let _ = std::fs::remove_file(self.bin.join(name));
    }
    /// Every tool installed and signed in.
    fn everything_signed_in(&self) {
        self.stub.reset();
        self.install(
            "gh",
            r#"case "$1 $2" in
  "auth token") printf 'fake-token';;
  "auth status") printf '{"hosts":{"github.com":[{"state":"success","active":true,"host":"github.com","login":"octo","tokenSource":"keyring"}]}}';;
  *) echo "gh version 2.83.0 (fake)";;
esac"#,
        );
        self.install("az", r#"if [ "$1" = "account" ]; then echo 'azure-user@example.com'; else echo 'azure-cli 2.60 (fake)'; fi"#);
        self.install("glab", r#"if [ "$1" = "auth" ]; then printf 'code.example.com\n  Logged in to code.example.com as corp-user (token)\n'; else echo 'glab 1.40 (fake)'; fi"#);
    }
    fn gh_signed_out(&self) {
        self.install("gh", "if [ \"$1\" = \"auth\" ]; then echo 'You are not logged in to any GitHub hosts. Run gh auth login to authenticate.' >&2; exit 1; fi\necho 'gh version 2.83.0 (fake)'");
    }
}

const OPERATE: [AuthEnvironmentScope; 3] = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::SettingsWrite];
const CLONE: [AuthEnvironmentScope; 4] = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::SettingsWrite, AuthEnvironmentScope::SourceControlWrite];

async fn open(fixture: &Fixture, scopes: &[AuthEnvironmentScope]) -> (VirtualDom, Harness, Store<UiModel>) {
    let (mut dom, props) = fixture.client(Gate::default(), scopes);
    let state = if scopes.contains(&AuthEnvironmentScope::OrchestrationOperate) {
        seed(&mut dom, &props, &fixture.workspace).await
    } else {
        let state = props.state.borrow().unwrap();
        until(&mut dom, "the shell", |_| state.peek().shell.synchronized).await;
        state
    };
    state.view().set(View::SourceControl);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    (dom, props, state)
}
fn disabled(dom: &VirtualDom, label: &str) -> bool {
    control_attribute(dom, label, "disabled").and_then(|(_, value)| value).as_deref() == Some("true")
}
fn setting(state: Store<UiModel>, path: &[&str]) -> Value {
    path.iter().fold(state.config().peek().clone(), |value, key| value[*key].clone())
}

#[tokio::test(flavor = "current_thread")]
async fn tools_and_hosts_are_listed_from_the_servers_own_scan_with_their_sign_in_and_a_rescan_follows_the_machine() {
    let world = world();
    let _alone = world.exclusive.lock().await;
    world.everything_signed_in();
    let fixture = Fixture::start(false).await;
    let (mut dom, _props, state) = open(&fixture, &OPERATE).await;
    wait_for_rendered_text(&mut dom, "Source Control Providers").await;
    let text = rendered_text(&dom);
    // Version control: Git with its version, and a tool the server does not support yet.
    assert!(text.contains("Version Control") && text.contains("Git") && text.contains("git version"), "{text}");
    assert!(text.contains("Support for Jujutsu is coming soon.") && text.contains("Coming Soon"));
    assert!(control(&dom, "Git availability").is_some() && control(&dom, "Jujutsu availability").is_none(), "an unsupported tool has no availability switch");
    // Providers: the four this server supports, Forgejo not among them.
    for provider in ["GitHub", "GitLab", "Azure DevOps", "Bitbucket"] {
        assert!(text.contains(provider), "{provider}");
    }
    assert!(!text.contains("Forgejo"));
    assert!(text.contains("Authenticated") && text.contains("gh version 2.83.0 (fake)"));
    assert!(text.contains("Available. Add a Bitbucket token in Settings"), "an API integration has no CLI to sign in with");
    // A login is hidden until asked for, and then shown.
    assert!(!text.contains("octo") && !text.contains("corp-user") && !text.contains("azure-user@example.com"), "logins are hidden: {text}");
    assert_eq!(control_attribute(&dom, "GitHub availability", "checked").unwrap().1.as_deref(), Some("true"));
    click_control(&mut dom, "Toggle source control account visibility");
    let text = rendered_text(&dom);
    assert!(text.contains("octo") && text.contains("corp-user") && text.contains("azure-user@example.com"));
    click_control(&mut dom, "Toggle source control account visibility");
    assert!(!rendered_text(&dom).contains("corp-user"));

    // gh is signed out, glab is gone: a rescan says so, with how to fix it.
    world.gh_signed_out();
    world.remove("glab");
    click_control(&mut dom, "Rescan server environment");
    wait_for_rendered_text(&mut dom, "GitHub is not authenticated on this server.").await;
    let text = rendered_text(&dom);
    assert!(text.contains("tool on the server host to enable change request features."));
    assert!(text.contains("Not available on this server:"), "{text}");
    assert_eq!(control_attribute(&dom, "GitHub availability", "checked").unwrap().1.as_deref(), Some("false"));
    assert!(text.contains("Not authenticated"), "a host that is installed and signed out is badged");
    assert!(!text.contains("octo"));
    // gh gone too: not installed.
    world.remove("gh");
    click_control(&mut dom, "Rescan server environment");
    until(&mut dom, "a rescan without gh", |dom| !rendered_text(dom).contains("GitHub is not authenticated on this server.")).await;
    assert_eq!(text_count(&dom, "Not available on this server:"), 2);
    assert!(state.peek().error.is_none());
    fixture.finish(dom).await;
}

fn text_count(dom: &VirtualDom, needle: &str) -> usize {
    rendered_text(dom).matches(needle).count()
}

#[tokio::test(flavor = "current_thread")]
async fn a_github_token_is_saved_write_only_used_by_the_scan_and_removed() {
    let world = world();
    let _alone = world.exclusive.lock().await;
    world.everything_signed_in();
    world.remove("gh");
    world.stub.route("/rest/github.com/user", 200, json!({"login":"token-user"}));
    let fixture = Fixture::start(false).await;
    let (mut dom, _props, state) = open(&fixture, &OPERATE).await;
    wait_for_rendered_text(&mut dom, "Source Control Providers").await;
    click_control(&mut dom, "Toggle GitHub details");
    assert!(rendered_text(&dom).contains("No token saved; the server uses GH_TOKEN or the gh login."));
    assert_eq!(control_attribute(&dom, "GitHub token", "placeholder").unwrap().1.as_deref(), Some("Not set"));
    assert!(disabled(&dom, "Save GitHub token") && control(&dom, "Remove GitHub token").is_none());
    input_control(&mut dom, "GitHub token host", "  GitHub.com ");
    input_control(&mut dom, "GitHub token", "ghp_very_secret");
    assert!(!disabled(&dom, "Save GitHub token"));
    click_control(&mut dom, "Save GitHub token");
    wait_for_rendered_text(&mut dom, "Saved for github.com.").await;
    let text = rendered_text(&dom);
    assert!(!text.contains("ghp_very_secret"), "a token is never shown");
    assert_ne!(setting(state, &["settings", "github", "tokens", "github.com"]), json!("ghp_very_secret"), "the server keeps it redacted");
    assert_eq!(setting(state, &["settings", "github", "tokens", "github.com"]), json!("••••••"));
    assert_eq!(control_attribute(&dom, "GitHub token", "placeholder").unwrap().1.as_deref(), Some("Stored secret, enter a new value to replace"));
    // The draft goes when the server has accepted the save, not when it was sent.
    until(&mut dom, "the draft to clear", |dom| control_attribute(dom, "GitHub token", "value").and_then(|(_, value)| value).unwrap_or_default().is_empty()).await;
    // The scan after the save used the token: gh is not installed, and GitHub is signed in anyway.
    wait_for_rendered_text(&mut dom, "Using the token saved in Settings").await;
    click_control(&mut dom, "Toggle source control account visibility");
    assert!(rendered_text(&dom).contains("token-user"));
    // Remove it.
    click_control(&mut dom, "Remove GitHub token");
    wait_for_rendered_text(&mut dom, "No token saved; the server uses GH_TOKEN or the gh login.").await;
    assert!(setting(state, &["settings", "github", "tokens", "github.com"]).is_null(), "the server drops an emptied token");
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn github_logins_are_chosen_per_host_and_defaults_leave_no_trace_in_settings() {
    let world = world();
    let _alone = world.exclusive.lock().await;
    world.everything_signed_in();
    world.install(
        "gh",
        r#"case "$1 $2" in
  "auth token") printf 'fake-token';;
  "auth status") printf '{"hosts":{"github.com":[{"state":"success","active":true,"host":"github.com","login":"octo","tokenSource":"keyring"},{"state":"success","active":false,"host":"github.com","login":"other","tokenSource":"keyring"}],"ghe.example.com":[{"state":"success","active":true,"host":"ghe.example.com","login":"corp","tokenSource":"keyring"}]}}';;
  *) echo "gh version 2.83.0 (fake)";;
esac"#,
    );
    let fixture = Fixture::start(false).await;
    let (mut dom, _props, state) = open(&fixture, &OPERATE).await;
    wait_for_rendered_text(&mut dom, "Source Control Providers").await;
    click_control(&mut dom, "Toggle GitHub details");
    until(&mut dom, "the accounts", |dom| control(dom, "GitHub account for github.com").is_some()).await;
    // Two logins on github.com can be chosen between; one on the other host is just shown.
    assert!(control(&dom, "GitHub account for ghe.example.com").is_none());
    assert_eq!(control_attribute(&dom, "GitHub account for github.com", "value").unwrap().1.as_deref(), Some("active gh account"));
    change_control(&mut dom, "GitHub account for github.com", "other");
    until(&mut dom, "the pin", |_| setting(state, &["settings", "github", "hosts"]) == json!({"github.com":{"enabled":true,"account":"other"}})).await;
    // Back to the active login leaves nothing behind.
    change_control(&mut dom, "GitHub account for github.com", "active gh account");
    until(&mut dom, "the pin gone", |_| setting(state, &["settings", "github", "hosts"]) == json!({})).await;
    // A host switched off is a real choice; switching it back on is not.
    change_control(&mut dom, "Use GitHub on ghe.example.com", "false");
    until(&mut dom, "the host off", |_| setting(state, &["settings", "github", "hosts"]) == json!({"ghe.example.com":{"enabled":false}})).await;
    change_control(&mut dom, "Use GitHub on ghe.example.com", "true");
    until(&mut dom, "the host on", |_| setting(state, &["settings", "github", "hosts"]) == json!({})).await;
    // The reveal switch shows the logins.
    click_control(&mut dom, "Reveal GitHub accounts");
    assert!(rendered_text(&dom).contains("corp"));
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn bitbucket_credentials_save_one_method_clear_the_other_and_never_come_back() {
    let world = world();
    let _alone = world.exclusive.lock().await;
    world.everything_signed_in();
    let fixture = Fixture::start(false).await;
    let (mut dom, _props, state) = open(&fixture, &OPERATE).await;
    wait_for_rendered_text(&mut dom, "Source Control Providers").await;
    click_control(&mut dom, "Toggle Bitbucket details");
    assert!(rendered_text(&dom).contains("Without a saved token, the server falls back to its T3CODE_BITBUCKET_* environment variables."));
    assert!(disabled(&dom, "Save Bitbucket credentials"));
    input_control(&mut dom, "Bitbucket access token", "bb-secret");
    world.stub.route("/bitbucket/user", 200, json!({"username":"bb-user"}));
    click_control(&mut dom, "Save Bitbucket credentials");
    until(&mut dom, "the saved token", |_| setting(state, &["settings", "bitbucket", "accessToken"]) == json!("••••••")).await;
    assert!(!rendered_text(&dom).contains("bb-secret"));
    assert_eq!(control_attribute(&dom, "Bitbucket access token", "placeholder").unwrap().1.as_deref(), Some("Stored secret, enter a new value to replace"));
    // The rescan after the save saw the stub accept it.
    until(&mut dom, "Bitbucket signed in", |dom| rendered_text(dom).contains("Authenticated")).await;
    // Choosing the other method warns that saving replaces the saved one, and does it.
    click_control(&mut dom, "Use api token");
    assert!(rendered_text(&dom).contains("Saving replaces your access token."));
    input_control(&mut dom, "Atlassian account email", "me@example.com");
    input_control(&mut dom, "Bitbucket API token", "api-secret");
    click_control(&mut dom, "Save Bitbucket credentials");
    until(&mut dom, "the api token", |_| setting(state, &["settings", "bitbucket"]) == json!({"email":"me@example.com","accessToken":"","apiToken":"••••••"})).await;
    assert!(!rendered_text(&dom).contains("api-secret"));
    // Changing only the email keeps the stored token.
    input_control(&mut dom, "Atlassian account email", "you@example.com");
    click_control(&mut dom, "Save Bitbucket credentials");
    until(&mut dom, "the new email", |_| setting(state, &["settings", "bitbucket", "email"]) == json!("you@example.com")).await;
    assert_eq!(setting(state, &["settings", "bitbucket", "apiToken"]), json!("••••••"));
    click_control(&mut dom, "Remove Bitbucket credentials");
    until(&mut dom, "no credentials", |_| setting(state, &["settings", "bitbucket"]) == json!({"email":"","accessToken":"","apiToken":""})).await;
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_connection_that_cannot_write_settings_reads_the_scan_and_changes_nothing() {
    let world = world();
    let _alone = world.exclusive.lock().await;
    world.everything_signed_in();
    let fixture = Fixture::start(false).await;
    let (mut dom, _props, state) = open(&fixture, &[AuthEnvironmentScope::OrchestrationRead]).await;
    wait_for_rendered_text(&mut dom, "Source Control Providers").await;
    assert!(rendered_text(&dom).contains("This connection can view source control tools but cannot change their settings."));
    click_control(&mut dom, "Toggle GitHub details");
    for label in ["GitHub token host", "GitHub token", "Save GitHub token"] {
        assert!(disabled(&dom, label), "{label}");
    }
    click_control(&mut dom, "Toggle Bitbucket details");
    for label in ["Bitbucket access token", "Save Bitbucket credentials", "Use api token"] {
        assert!(disabled(&dom, label), "{label}");
    }
    // Reading still works: a rescan is allowed.
    assert!(!disabled(&dom, "Rescan server environment"));
    assert!(state.peek().error.is_none());
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_lost_connection_marks_the_scan_out_of_date_and_stops_saving() {
    let world = world();
    let _alone = world.exclusive.lock().await;
    world.everything_signed_in();
    let fixture = Fixture::start(false).await;
    let (mut dom, props, state) = open(&fixture, &OPERATE).await;
    wait_for_rendered_text(&mut dom, "Source Control Providers").await;
    click_control(&mut dom, "Toggle GitHub details");
    assert!(!rendered_text(&dom).contains("may be out of date"));
    dom.in_scope(ScopeId::APP, || interrupt_connection(&props.transport, state, "the network went away"));
    wait_for_rendered_text(&mut dom, "This scan may be out of date.").await;
    assert!(rendered_text(&dom).contains("Source Control Providers"), "the last scan stays");
    for label in ["Rescan server environment", "Save GitHub token", "GitHub token"] {
        assert!(disabled(&dom, label), "{label}");
    }
    assert!(state.peek().error.is_none());
    fixture.finish(dom).await;
}

fn seed_repository(base: &Path) -> PathBuf {
    let bare = base.join("web.git");
    std::fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "--bare", "-q"]);
    git(&bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    let seed = base.join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    git(&seed, &["init", "-q"]);
    std::fs::write(seed.join("README.md"), "hello\n").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-q", "-m", "seed"]);
    git(&seed, &["push", bare.to_str().unwrap(), "HEAD:refs/heads/main"]);
    bare
}

#[tokio::test(flavor = "current_thread")]
async fn a_repository_is_cloned_into_a_new_project_from_a_git_url_or_a_looked_up_host() {
    let world = world();
    let _alone = world.exclusive.lock().await;
    world.everything_signed_in();
    let fixture = Fixture::start(false).await;
    let base = std::fs::canonicalize(fixture.directory.path()).unwrap();
    let bare = seed_repository(&base);
    // GitLab's repository lookup answers from the fake glab with an SSH URL that is a local path,
    // which GitLab clones by default; GitHub's would be an https URL, so it stops before cloning.
    world.install(
        "glab",
        &format!(
            r#"case "$1" in
  api) printf '{{"path_with_namespace":"group/web","web_url":"https://gitlab.invalid/group/web","http_url_to_repo":"https://gitlab.invalid/group/web.git","ssh_url_to_repo":"{}"}}';;
  auth) printf 'code.example.com\n  Logged in to code.example.com as corp-user (token)\n';;
  *) echo 'glab 1.40 (fake)';;
esac"#,
            bare.display()
        ),
    );
    world.stub.route("/rest/github.com/repos/acme/web", 200, json!({"full_name":"acme/web","html_url":"https://github.com/acme/web","ssh_url":bare,"default_branch":"main"}));
    let (mut dom, _props, state) = open(&fixture, &CLONE).await;
    state.view().set(View::Chat);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);

    // The sources, with the ready providers first and a provider that is not ready explained.
    world.remove("az");
    open_palette(&mut dom).await;
    input_control(&mut dom, PALETTE, "clone");
    until(&mut dom, "the clone action", |dom| control(dom, "Clone repository").is_some()).await;
    click_control(&mut dom, "Clone repository");
    until(&mut dom, "the sources", |dom| control(dom, "Git URL").is_some() && rendered_text(dom).contains("Setup required."), ).await;
    assert!(control(&dom, "GitHub repository").is_some() && control(&dom, "GitLab repository").is_some() && control(&dom, "Bitbucket repository").is_some());
    assert!(disabled(&dom, "Azure DevOps repository"), "a provider that is not installed cannot be chosen");
    assert!(rendered_text(&dom).contains("Setup required. Install the Azure CLI") || rendered_text(&dom).contains("Setup required."));
    assert!(control(&dom, "Open source control settings").is_some());

    // A Git URL needs no lookup: the destination is proposed, edited, and cloned into a project.
    let destination = base.join("clones").join("by-url");
    click_control(&mut dom, "Git URL");
    until(&mut dom, "the URL step", |dom| control(dom, "Enter Git clone URL").is_some()).await;
    input_control(&mut dom, "Enter Git clone URL", bare.to_str().unwrap());
    click_control(&mut dom, "Look up repository");
    until(&mut dom, "the destination step", |dom| control(dom, "Clone destination path").is_some()).await;
    assert_eq!(control_attribute(&dom, "Clone destination path", "value").unwrap().1.as_deref(), Some("~/web"), "the repository's own folder in the default parent");
    input_control(&mut dom, "Clone destination path", "../relative/place");
    click_control(&mut dom, "Clone repository");
    wait_for_rendered_text(&mut dom, "Relative paths need an active project; enter the full path.").await;
    input_control(&mut dom, "Clone destination path", destination.to_str().unwrap());
    click_control(&mut dom, "Clone repository");
    until(&mut dom, "the project", |_| state.peek().shell.snapshot.as_ref().is_some_and(|snapshot| snapshot.projects.iter().any(|project| project.workspace_root.as_str() == destination.to_str().unwrap()))).await;
    // The project exists before git is done; the form it opens on shows the clone, and nothing once it is done.
    until(&mut dom, "the files", |_| destination.join("README.md").exists()).await;
    assert_eq!(std::fs::read_to_string(destination.join("README.md")).unwrap(), "hello\n");
    // The closed palette and the new project's thread form follow the project.
    assert!(control(&dom, PALETTE).is_none());

    // A looked-up host: GitLab, by its own name for the repository.
    open_palette(&mut dom).await;
    input_control(&mut dom, PALETTE, "clone");
    until(&mut dom, "the clone action", |dom| control(dom, "Clone repository").is_some()).await;
    click_control(&mut dom, "Clone repository");
    until(&mut dom, "the sources", |dom| control(dom, "GitLab repository").is_some() && !disabled(dom, "GitLab repository")).await;
    click_control(&mut dom, "GitLab repository");
    until(&mut dom, "the repository step", |dom| control(dom, "Enter GitLab repository (group/project)").is_some()).await;
    input_control(&mut dom, "Enter GitLab repository (group/project)", " group/web ");
    key_on(&mut dom, "Enter GitLab repository (group/project)", Key::Enter);
    until(&mut dom, "the destination step", |dom| control(dom, "Clone destination path").is_some()).await;
    let second = base.join("clones").join("from-gitlab");
    input_control(&mut dom, "Clone destination path", second.to_str().unwrap());
    key_on(&mut dom, "Clone destination path", Key::Enter);
    until(&mut dom, "the second project", |_| second.join("README.md").exists() && state.peek().shell.snapshot.as_ref().is_some_and(|snapshot| snapshot.projects.iter().any(|project| project.workspace_root.as_str() == second.to_str().unwrap()))).await;

    // The closed palette comes last; a key pressed before it would toggle the palette shut.
    until(&mut dom, "the palette closed", |dom| control(dom, PALETTE).is_none()).await;

    // A lookup that fails says so in the palette, where the user is looking; GitHub's repository
    // would clone over https, so the step after the lookup is only checked.
    open_palette(&mut dom).await;
    input_control(&mut dom, PALETTE, "clone");
    until(&mut dom, "the clone action", |dom| control(dom, "Clone repository").is_some()).await;
    click_control(&mut dom, "Clone repository");
    until(&mut dom, "the sources", |dom| control(dom, "GitHub repository").is_some() && !disabled(dom, "GitHub repository")).await;
    click_control(&mut dom, "GitHub repository");
    until(&mut dom, "the repository step", |dom| control(dom, "Enter GitHub repository (owner/repo)").is_some()).await;
    world.stub.route("/rest/github.com/repos/acme/gone", 404, json!({"message":"Not Found"}));
    input_control(&mut dom, "Enter GitHub repository (owner/repo)", "acme/gone");
    click_control(&mut dom, "Look up repository");
    wait_for_rendered_text(&mut dom, "Repository lookup failed.").await;
    input_control(&mut dom, "Enter GitHub repository (owner/repo)", "acme/web");
    click_control(&mut dom, "Look up repository");
    until(&mut dom, "the destination step", |dom| control(dom, "Clone destination path").is_some()).await;
    assert_eq!(control_attribute(&dom, "Clone destination path", "value").unwrap().1.as_deref(), Some("~/web"));
    // Backspace on an empty box goes back a step, and Escape closes the palette.
    input_control(&mut dom, "Clone destination path", "");
    key_on(&mut dom, "Clone destination path", Key::Backspace);
    until(&mut dom, "the sources again", |dom| control(dom, "GitHub repository").is_some()).await;
    key_on(&mut dom, "Search...", Key::Escape);
    assert!(control(&dom, "Search...").is_none());
    assert_eq!(world.stub.seen.lock().unwrap().iter().filter(|(_, path, _)| path == "/rest/github.com/repos/acme/web").count(), 1);
    assert!(state.peek().error.is_none());
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn cloning_needs_source_control_and_operate_grants() {
    let world = world();
    let _alone = world.exclusive.lock().await;
    world.everything_signed_in();
    let fixture = Fixture::start(false).await;
    let (mut dom, _props, state) = open(&fixture, &OPERATE).await;
    state.view().set(View::Chat);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    open_palette(&mut dom).await;
    // OPERATE lacks the source control grant, so the action is not offered.
    input_control(&mut dom, PALETTE, ">clone");
    until(&mut dom, "an empty filtered list", |dom| rendered_text(dom).contains("No matching actions.")).await;
    assert!(control(&dom, "Clone repository").is_none());
    fixture.finish(dom).await;
}
