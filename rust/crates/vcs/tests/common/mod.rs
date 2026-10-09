//! Shared helpers for the tests that drive a real `git` in temporary repositories. Git only
//! ever runs inside directories created here, with the machine's git configuration disabled.
#![allow(dead_code)]
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::{Duration, UNIX_EPOCH},
};

use futures_util::future::BoxFuture;
use t3_vcs::{
    GitVcs, VcsProcess,
    process::{
        ProcessRunError, ProcessRunInput, ProcessRunOutput, ProcessRunner, TokioProcessRunner,
    },
};

pub type RunResult = Result<ProcessRunOutput, ProcessRunError>;

const HERMETIC_ENV: [(&str, &str); 3] = [
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_TERMINAL_PROMPT", "0"),
];

/// The real runner, with the host's git configuration switched off.
#[derive(Clone, Copy)]
pub struct Live;

impl Live {
    pub fn run(self, mut input: ProcessRunInput) -> BoxFuture<'static, RunResult> {
        let mut env: Vec<_> = HERMETIC_ENV
            .iter()
            .map(|(name, value)| (name.to_string(), Some(value.to_string())))
            .collect();
        env.append(&mut input.env);
        input.env = env;
        Box::pin(async move { TokioProcessRunner.run(input).await })
    }
}

/// `Live` as a runner, for drivers that take a `ProcessRunner` directly.
pub struct LiveRunner;

impl ProcessRunner for LiveRunner {
    fn run(&self, input: ProcessRunInput) -> BoxFuture<'_, RunResult> {
        Live.run(input)
    }
}

pub fn live_core() -> t3_vcs::core::GitCore {
    t3_vcs::core::GitCore::new(Arc::new(LiveRunner))
}

struct Intercept<F>(F);

impl<F> ProcessRunner for Intercept<F>
where
    F: Fn(ProcessRunInput) -> BoxFuture<'static, RunResult> + Send + Sync + 'static,
{
    fn run(&self, input: ProcessRunInput) -> BoxFuture<'_, RunResult> {
        (self.0)(input)
    }
}

/// A driver whose every process goes through `intercept`, which usually delegates to `Live`.
pub fn git_with(
    intercept: impl Fn(ProcessRunInput) -> BoxFuture<'static, RunResult> + Send + Sync + 'static,
) -> GitVcs {
    // No pause between retries: the tests that expect a retry must not wait for one.
    GitVcs::new(
        VcsProcess::with_runner(Arc::new(Intercept(intercept))).with_retry_delay(Duration::ZERO),
    )
}

/// Like `git_with`, with explicit pacing for the retry and recovery deadlines.
pub fn git_with_timing(
    intercept: impl Fn(ProcessRunInput) -> BoxFuture<'static, RunResult> + Send + Sync + 'static,
    retry_delay: Duration,
    recovery_timeout: Duration,
) -> GitVcs {
    GitVcs::new(
        VcsProcess::with_runner(Arc::new(Intercept(intercept))).with_retry_delay(retry_delay),
    )
    .with_checkpoint_recovery_timeout(recovery_timeout)
}

pub fn live_git() -> GitVcs {
    git_with(|input| Live.run(input))
}

pub fn run_git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .envs(HERMETIC_ENV)
        // Fixed dates keep commits made by scenarios (and so gitlinks to them) reproducible.
        .env("GIT_AUTHOR_DATE", "2020-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2020-01-01T00:00:00Z")
        .output()
        .expect("git can be spawned");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

pub fn has_arg(input: &ProcessRunInput, arg: &str) -> bool {
    input.args.iter().any(|candidate| candidate == arg)
}

pub fn env_value(input: &ProcessRunInput, name: &str) -> Option<String> {
    input
        .env
        .iter()
        .rev()
        .find(|(candidate, _)| candidate == name)
        .and_then(|(_, value)| value.clone())
}

pub struct Repo {
    pub dir: tempfile::TempDir,
}

impl Repo {
    /// An empty directory, not yet a repository.
    pub fn bare_dir() -> Self {
        Self {
            dir: tempfile::Builder::new()
                .prefix("t3-vcs-test-")
                .tempdir()
                .unwrap(),
        }
    }

    /// A repository with one commit (`README.md`) and a configured identity.
    pub fn with_commit() -> Self {
        let repo = Self::init();
        repo.write("README.md", "# test\n");
        repo.git(&["add", "."]);
        repo.git(&["commit", "-m", "initial commit"]);
        repo
    }

    pub fn init() -> Self {
        let repo = Self::bare_dir();
        repo.git(&["init"]);
        repo.git(&["config", "user.email", "test@test.com"]);
        repo.git(&["config", "user.name", "Test"]);
        repo
    }

    /// The fixture of the original checkpoint tests: a committed `file.txt`, then a staged edit
    /// and a different working-tree edit of the same file.
    pub fn checkpoint_fixture() -> Self {
        let repo = Self::init();
        repo.write("file.txt", "initial\n");
        repo.git(&["add", "."]);
        repo.git(&["commit", "-m", "initial"]);
        repo.write("file.txt", "staged\n");
        repo.git(&["add", "."]);
        repo.write("file.txt", "unstaged\n");
        repo
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn join(&self, relative: &str) -> PathBuf {
        self.path().join(relative)
    }

    pub fn git(&self, args: &[&str]) -> String {
        run_git(self.path(), args)
    }

    pub fn git_in(&self, relative: &str, args: &[&str]) -> String {
        run_git(&self.join(relative), args)
    }

    pub fn write(&self, relative: &str, contents: &str) {
        let path = self.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    pub fn read(&self, relative: &str) -> String {
        fs::read_to_string(self.join(relative)).unwrap()
    }

    pub fn remove(&self, relative: &str) {
        fs::remove_file(self.join(relative)).unwrap();
    }

    pub fn exists(&self, relative: &str) -> bool {
        self.join(relative).exists()
    }

    pub fn index_bytes(&self) -> Option<Vec<u8>> {
        fs::read(self.join(".git/index")).ok()
    }

    /// Contents of `<ref>:<path>` in the object database.
    pub fn show(&self, revision: &str, path: &str) -> String {
        self.git(&["show", &format!("{revision}:{path}")])
    }
}

pub fn set_times(path: &Path, seconds: f64) {
    let time = UNIX_EPOCH + Duration::from_secs_f64(seconds);
    let file = fs::OpenOptions::new().write(true).open(path).unwrap();
    file.set_times(fs::FileTimes::new().set_accessed(time).set_modified(time))
        .unwrap();
}

/// A core whose every process goes through `intercept` (usually delegating to `Live`), with
/// caches and throttles on `clock`.
pub fn core_with(
    clock: Arc<dyn t3_vcs::Clock>,
    intercept: impl Fn(ProcessRunInput) -> BoxFuture<'static, RunResult> + Send + Sync + 'static,
) -> t3_vcs::core::GitCore {
    t3_vcs::core::GitCore::with_clock(
        Arc::new(Intercept(intercept)),
        clock,
        t3_vcs::GitTimings::default(),
    )
}

/// The arguments of every git process a core has run so far.
#[derive(Clone, Default)]
pub struct Calls(Arc<std::sync::Mutex<Vec<Vec<String>>>>);

impl Calls {
    pub fn record(&self, input: &ProcessRunInput) {
        self.0.lock().unwrap().push(input.args.clone());
    }

    /// How many recorded invocations have all of `needles` among their arguments.
    pub fn count(&self, needles: &[&str]) -> usize {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|args| {
                needles
                    .iter()
                    .all(|needle| args.iter().any(|arg| arg == needle))
            })
            .count()
    }

    pub fn clear(&self) {
        self.0.lock().unwrap().clear();
    }
}

/// A core on a manual clock that records every git invocation and otherwise runs it live.
pub fn counting_core() -> (t3_vcs::core::GitCore, Arc<t3_vcs::ManualClock>, Calls) {
    let (clock, calls) = (Arc::new(t3_vcs::ManualClock::default()), Calls::default());
    let recorder = calls.clone();
    let core = core_with(clock.clone(), move |input| {
        recorder.record(&input);
        Live.run(input)
    });
    (core, clock, calls)
}

/// What a failed git process looks like to the runner.
pub fn failed_process(code: i32, stderr: &str) -> RunResult {
    Ok(ProcessRunOutput {
        code: Some(code),
        stderr: stderr.into(),
        ..ProcessRunOutput::default()
    })
}

/// A bare repository to use as a remote.
pub fn bare_remote() -> Repo {
    let remote = Repo::bare_dir();
    remote.git(&["init", "--bare"]);
    remote
}

/// A clone of `remote` with an identity configured.
pub fn clone_of(remote: &Repo) -> Repo {
    let peer = Repo::bare_dir();
    peer.git(&["clone", &remote.path().to_string_lossy(), "."]);
    peer.git(&["config", "user.email", "test@test.com"]);
    peer.git(&["config", "user.name", "Test"]);
    peer
}

impl Repo {
    /// Commits a file in one step.
    pub fn commit_file(&self, path: &str, contents: &str, message: &str) {
        self.write(path, contents);
        self.git(&["add", path]);
        self.git(&["commit", "-m", message]);
    }

    pub fn branch(&self) -> String {
        self.git(&["branch", "--show-current"]).trim().to_string()
    }

    /// A repository with one commit and a bare `origin` it has pushed its branch to.
    pub fn with_origin() -> (Self, Self) {
        let repo = Self::with_commit();
        let remote = bare_remote();
        repo.git(&["remote", "add", "origin", &remote.path().to_string_lossy()]);
        repo.git(&["push", "-u", "origin", &repo.branch()]);
        (repo, remote)
    }
}
