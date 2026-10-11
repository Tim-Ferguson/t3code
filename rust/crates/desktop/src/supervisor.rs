//! Keeps the T3 Code server this app started running (`DesktopBackendManager.ts`): starts it, notices that it
//! answers, restarts it when it dies with a growing delay, gives up and says so after repeated failures, and
//! stops it when the app quits.
//!
//! The process and the way to start one are traits, so the whole life cycle is tested with a fake child and
//! with time that only moves when the test runtime has nothing else to do.
use std::{future::Future, time::Duration};
use tokio::sync::mpsc;

/// A credential that does not print itself.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(pub String);
impl std::fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Secret(..)")
    }
}

/// What the server is doing, as the window shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerState {
    /// Started and not yet answering. `attempt` is 1 for a start after the server answered (or the first start)
    /// and one more for each failure since.
    Starting { attempt: u32 },
    /// Answering at `address`. `credential` is this start's bootstrap credential, good for one pairing.
    /// `launch` counts the starts of this app, so a window can tell a restarted server from the same one.
    Ready { address: String, credential: Secret, launch: u32 },
    /// Died or never answered; the next start is `in_ms` away. `attempt` counts the failures in a row.
    Restarting { attempt: u32, in_ms: u64, reason: String },
    /// Gave up after repeated failures. `Command::Retry` tries again.
    Failed { reason: String },
    /// Stopped for good: the app is quitting.
    Stopped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Stop,
    Retry,
    /// Ends the running server and starts a new one at once, as a first start (a server that serves only the failure of
    /// its import is what this is for: fixing the data does not change what it serves).
    Restart,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    /// The delay before the first restart; each next one doubles it (`INITIAL_RESTART_DELAY`).
    pub initial_delay: Duration,
    /// The longest delay (`MAX_RESTART_DELAY`).
    pub max_delay: Duration,
    /// How long a started server may take to answer (`DEFAULT_BACKEND_READINESS_TIMEOUT`).
    pub readiness_timeout: Duration,
    /// How long a server asked to stop may take before it is ended (the 5 s of `stopAllPoolInstances`).
    pub stop_grace: Duration,
    /// Starts in a row that fail before the supervisor stops trying. A start that answers resets the count.
    pub give_up_after: u32,
}
impl Default for Timing {
    fn default() -> Self {
        Self { initial_delay: Duration::from_millis(500), max_delay: Duration::from_secs(10), readiness_timeout: Duration::from_secs(60), stop_grace: Duration::from_secs(5), give_up_after: 6 }
    }
}
impl Timing {
    /// The wait after the `failures`th failure in a row (`calculateRestartDelay`).
    pub fn delay(&self, failures: u32) -> Duration {
        let doubled = self.initial_delay.saturating_mul(2u32.saturating_pow(failures.saturating_sub(1)));
        doubled.min(self.max_delay)
    }
}

/// How a process ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exit(pub String);

pub trait Process: Send + 'static {
    /// Resolves when the process has ended. Cancel-safe: dropping the future loses nothing.
    fn wait(&mut self) -> impl Future<Output = Exit> + Send;
    /// Asks it to shut down.
    fn terminate(&mut self);
    /// Ends it now.
    fn kill(&mut self);
}

pub struct Launched<P> {
    pub process: P,
    pub address: String,
    pub credential: Secret,
}

pub trait Launcher: Send + 'static {
    type Proc: Process;
    /// Starts the server, or says why that is not possible (no such program, no free port).
    fn launch(&mut self, attempt: u32) -> Result<Launched<Self::Proc>, String>;
    /// Resolves once the server started by the last `launch` answers; never if it does not. The address, when the
    /// server named one that differs from `address`, is where it answers.
    fn ready(&mut self, address: &str) -> impl Future<Output = Option<String>> + Send;
    /// What the server said just before it ended, for the failure message.
    fn last_words(&mut self) -> Option<String> {
        None
    }
}

/// Waits for a stop or a restart. A closed command channel means nobody is left to ask for anything: a stop.
async fn interruption(commands: &mut mpsc::UnboundedReceiver<Command>) -> Command {
    while let Some(command) = commands.recv().await {
        if command != Command::Retry {
            return command;
        }
    }
    Command::Stop
}

async fn shut_down<P: Process>(process: &mut P, grace: Duration) {
    process.terminate();
    if tokio::time::timeout(grace, process.wait()).await.is_err() {
        process.kill();
        process.wait().await;
    }
}

enum Phase {
    Answered(Option<String>),
    Ended(Exit),
    TimedOut,
    Interrupted(Command),
}

/// Runs until stopped. `publish` receives every state, in order.
pub async fn run<L: Launcher>(mut launcher: L, timing: Timing, mut commands: mpsc::UnboundedReceiver<Command>, mut publish: impl FnMut(ServerState) + Send) {
    let mut failures: u32 = 0;
    let mut launch: u32 = 0;
    loop {
        publish(ServerState::Starting { attempt: failures + 1 });
        let failure = match launcher.launch(failures + 1) {
            Err(reason) => reason,
            Ok(Launched { mut process, address, credential }) => {
                launch += 1;
                let phase = tokio::select! {
                    biased;
                    command = interruption(&mut commands) => Phase::Interrupted(command),
                    exit = process.wait() => Phase::Ended(exit),
                    named = launcher.ready(&address) => Phase::Answered(named),
                    () = tokio::time::sleep(timing.readiness_timeout) => Phase::TimedOut,
                };
                let mut reason = match phase {
                    Phase::Interrupted(command) => {
                        shut_down(&mut process, timing.stop_grace).await;
                        if command == Command::Restart {
                            failures = 0;
                            continue;
                        }
                        publish(ServerState::Stopped);
                        return;
                    }
                    Phase::TimedOut => {
                        shut_down(&mut process, timing.stop_grace).await;
                        format!("It did not answer within {} seconds.", timing.readiness_timeout.as_secs())
                    }
                    Phase::Ended(exit) => format!("It stopped before it answered ({}).", exit.0),
                    Phase::Answered(named) => {
                        failures = 0;
                        publish(ServerState::Ready { address: named.unwrap_or(address), credential, launch });
                        tokio::select! {
                            biased;
                            command = interruption(&mut commands) => {
                                shut_down(&mut process, timing.stop_grace).await;
                                if command == Command::Restart {
                                    continue;
                                }
                                publish(ServerState::Stopped);
                                return;
                            }
                            exit = process.wait() => format!("It stopped ({}).", exit.0),
                        }
                    }
                };
                if let Some(words) = launcher.last_words() {
                    reason = format!("{reason} {words}");
                }
                reason
            }
        };
        failures += 1;
        if failures >= timing.give_up_after {
            publish(ServerState::Failed { reason: failure });
            loop {
                match commands.recv().await {
                    Some(Command::Retry | Command::Restart) => break,
                    Some(Command::Stop) | None => {
                        publish(ServerState::Stopped);
                        return;
                    }
                }
            }
            failures = 0;
            continue;
        }
        let delay = timing.delay(failures);
        publish(ServerState::Restarting { attempt: failures, in_ms: delay.as_millis() as u64, reason: failure });
        tokio::select! {
            biased;
            command = interruption(&mut commands) => {
                if command == Command::Restart {
                    failures = 0;
                    continue;
                }
                publish(ServerState::Stopped);
                return;
            }
            () = tokio::time::sleep(delay) => {}
        }
    }
}

/// The supervisor running on a thread of its own, as the window holds it.
#[derive(Clone)]
pub struct Handle {
    commands: mpsc::UnboundedSender<Command>,
    state: tokio::sync::watch::Receiver<ServerState>,
    finished: std::sync::Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
}

impl Handle {
    /// Starts supervising on a new thread. With `exit_on_signal` the process, when interrupted or asked to
    /// terminate (Unix), stops the server first and then exits: a server must not outlive the app that started it.
    pub fn spawn<L: Launcher>(launcher: L, timing: Timing, exit_on_signal: bool) -> std::io::Result<Self> {
        let (commands, receiver) = mpsc::unbounded_channel();
        let (sender, state) = tokio::sync::watch::channel(ServerState::Starting { attempt: 1 });
        let finished = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let (thread_finished, thread_commands) = (finished.clone(), commands.clone());
        let signalled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread_signalled = signalled.clone();
        std::thread::Builder::new().name("t3-server-supervisor".into()).spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("a runtime for the supervisor");
            runtime.block_on(async move {
                if exit_on_signal {
                    #[cfg(unix)]
                    tokio::spawn(async move {
                        use tokio::signal::unix::{SignalKind, signal};
                        if let (Ok(mut terminate), Ok(mut interrupt)) = (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) {
                            tokio::select! { _ = terminate.recv() => {}, _ = interrupt.recv() => {} }
                            thread_signalled.store(true, std::sync::atomic::Ordering::SeqCst);
                            let _ = thread_commands.send(Command::Stop);
                        }
                    });
                }
                run(launcher, timing, receiver, move |state| {
                    let _ = sender.send(state);
                })
                .await;
            });
            let (done, wake) = &*thread_finished;
            *done.lock().unwrap() = true;
            wake.notify_all();
            if signalled.load(std::sync::atomic::Ordering::SeqCst) {
                // The server is stopped; the signal that ended it was meant for the whole app.
                std::process::exit(0);
            }
        })?;
        Ok(Self { commands, state, finished })
    }

    /// The state now and as it changes.
    pub fn state(&self) -> tokio::sync::watch::Receiver<ServerState> {
        self.state.clone()
    }

    pub fn retry(&self) {
        let _ = self.commands.send(Command::Retry);
    }

    /// Ends the running server and starts another.
    pub fn restart(&self) {
        let _ = self.commands.send(Command::Restart);
    }

    /// Stops the server and waits for it to have stopped, up to `timeout`. True if it had.
    pub fn stop_blocking(&self, timeout: Duration) -> bool {
        let _ = self.commands.send(Command::Stop);
        let (done, wake) = &*self.finished;
        let guard = done.lock().unwrap();
        *wake.wait_timeout_while(guard, timeout, |done| !*done).unwrap().0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };
    use tokio::{
        sync::{oneshot, watch},
        time::Instant,
    };

    /// What one start of the fake server does.
    enum Script {
        /// Answers at once and runs until `crash` is called on its handle.
        Healthy,
        /// Ends at once with this description, before answering.
        Dies(&'static str),
        /// Runs but never answers.
        Silent,
        /// Cannot be started.
        CannotStart(&'static str),
        /// Answers, and ignores the request to stop, so that it has to be ended.
        Stubborn,
    }

    #[derive(Default)]
    struct Record {
        launches: u32,
        terminated: u32,
        killed: u32,
        /// Ends the running fake process, as a crash.
        crash: Option<oneshot::Sender<Exit>>,
    }

    struct Fake {
        ends: Option<oneshot::Receiver<Exit>>,
        record: Arc<Mutex<Record>>,
        stubborn: bool,
        ended: Option<Exit>,
        killed: watch::Sender<bool>,
    }
    impl Process for Fake {
        async fn wait(&mut self) -> Exit {
            if let Some(exit) = &self.ended {
                return exit.clone();
            }
            let mut killed = self.killed.subscribe();
            let exit = tokio::select! {
                exit = self.ends.as_mut().expect("a process that has not ended has its receiver") => exit.unwrap_or(Exit("closed".into())),
                _ = killed.wait_for(|killed| *killed) => Exit("killed".into()),
            };
            self.ended = Some(exit.clone());
            exit
        }
        fn terminate(&mut self) {
            self.record.lock().unwrap().terminated += 1;
            if !self.stubborn {
                let sender = self.record.lock().unwrap().crash.take();
                if let Some(sender) = sender {
                    let _ = sender.send(Exit("terminated".into()));
                }
            }
        }
        fn kill(&mut self) {
            self.record.lock().unwrap().killed += 1;
            self.killed.send_replace(true);
        }
    }

    struct FakeLauncher {
        scripts: VecDeque<Script>,
        record: Arc<Mutex<Record>>,
        answers: bool,
        /// The address the next answer names, when it differs from the one the server was started for.
        names: Option<String>,
    }
    impl Launcher for FakeLauncher {
        type Proc = Fake;
        fn launch(&mut self, _attempt: u32) -> Result<Launched<Fake>, String> {
            let script = self.scripts.pop_front().unwrap_or(Script::Healthy);
            self.record.lock().unwrap().launches += 1;
            let number = self.record.lock().unwrap().launches;
            let (sender, receiver) = oneshot::channel();
            let (killed, _) = watch::channel(false);
            let stubborn = matches!(script, Script::Stubborn);
            self.answers = matches!(script, Script::Healthy | Script::Stubborn);
            match script {
                Script::CannotStart(why) => return Err(why.into()),
                Script::Dies(how) => {
                    let _ = sender.send(Exit(how.into()));
                }
                _ => self.record.lock().unwrap().crash = Some(sender),
            }
            Ok(Launched { process: Fake { ends: Some(receiver), record: self.record.clone(), stubborn, ended: None, killed }, address: "http://127.0.0.1:3773".into(), credential: Secret(format!("credential-{number}")) })
        }
        async fn ready(&mut self, _address: &str) -> Option<String> {
            if !self.answers {
                std::future::pending::<()>().await;
            }
            self.names.take()
        }
    }

    struct Harness {
        commands: mpsc::UnboundedSender<Command>,
        states: mpsc::UnboundedReceiver<(ServerState, Instant)>,
        record: Arc<Mutex<Record>>,
        task: tokio::task::JoinHandle<()>,
    }
    impl Harness {
        fn start(scripts: Vec<Script>, timing: Timing) -> Self {
            let record = Arc::new(Mutex::new(Record::default()));
            let (commands, command_receiver) = mpsc::unbounded_channel();
            let (sender, states) = mpsc::unbounded_channel();
            let launcher = FakeLauncher { scripts: scripts.into(), record: record.clone(), answers: false, names: None };
            let task = tokio::spawn(run(launcher, timing, command_receiver, move |state| {
                let _ = sender.send((state, Instant::now()));
            }));
            Self { commands, states, record, task }
        }
        async fn next(&mut self) -> (ServerState, Instant) {
            self.states.recv().await.expect("the supervisor publishes until it stops")
        }
        async fn state(&mut self) -> ServerState {
            self.next().await.0
        }
        fn crash(&self) {
            let sender = self.record.lock().unwrap().crash.take().expect("a running process");
            let _ = sender.send(Exit("exit code 101".into()));
        }
    }
    fn starting(attempt: u32) -> ServerState {
        ServerState::Starting { attempt }
    }

    #[test]
    fn the_delay_doubles_from_half_a_second_to_ten_and_stays_there() {
        let timing = Timing::default();
        let delays: Vec<_> = (1..=7).map(|failures| timing.delay(failures).as_millis()).collect();
        assert_eq!(delays, [500, 1000, 2000, 4000, 8000, 10_000, 10_000]);
        assert_eq!(timing.delay(0).as_millis(), 500);
        assert_eq!(timing.delay(u32::MAX).as_millis(), 10_000, "no overflow");
    }

    #[tokio::test(start_paused = true)]
    async fn a_server_that_answers_is_ready_with_this_starts_credential_and_stops_when_asked() {
        let mut run = Harness::start(vec![Script::Healthy], Timing::default());
        assert_eq!(run.state().await, starting(1));
        assert_eq!(run.state().await, ServerState::Ready { address: "http://127.0.0.1:3773".into(), credential: Secret("credential-1".into()), launch: 1 });
        run.commands.send(Command::Stop).unwrap();
        assert_eq!(run.state().await, ServerState::Stopped);
        run.task.await.unwrap();
        let record = run.record.lock().unwrap();
        assert_eq!((record.launches, record.terminated, record.killed), (1, 1, 0), "asked to stop, and it did");
    }

    #[tokio::test(start_paused = true)]
    async fn a_crash_restarts_with_growing_delays_and_an_answer_starts_the_count_again() {
        let mut run = Harness::start(vec![Script::Healthy, Script::Dies("exit code 1"), Script::Dies("exit code 1"), Script::Healthy], Timing::default());
        assert_eq!(run.state().await, starting(1));
        assert!(matches!(run.state().await, ServerState::Ready { launch: 1, .. }));
        run.crash();
        // The first failure after an answer waits the initial delay.
        let (restarting, at) = run.next().await;
        assert_eq!(restarting, ServerState::Restarting { attempt: 1, in_ms: 500, reason: "It stopped (exit code 101).".into() });
        let (state, next_at) = run.next().await;
        assert_eq!(state, starting(2));
        assert_eq!(next_at - at, Duration::from_millis(500));
        // Dies at once, twice: 1 s, then 2 s.
        let (restarting, at) = run.next().await;
        assert_eq!(restarting, ServerState::Restarting { attempt: 2, in_ms: 1000, reason: "It stopped before it answered (exit code 1).".into() });
        let (state, next_at) = run.next().await;
        assert_eq!(state, starting(3));
        assert_eq!(next_at - at, Duration::from_secs(1));
        let (restarting, at) = run.next().await;
        assert!(matches!(restarting, ServerState::Restarting { attempt: 3, in_ms: 2000, .. }));
        let (state, next_at) = run.next().await;
        assert_eq!(state, starting(4));
        assert_eq!(next_at - at, Duration::from_secs(2));
        // It answers, so the next failure is a first one again.
        assert!(matches!(run.state().await, ServerState::Ready { launch: 4, .. }));
        run.crash();
        assert!(matches!(run.state().await, ServerState::Restarting { attempt: 1, in_ms: 500, .. }));
        run.commands.send(Command::Stop).unwrap();
        assert_eq!(run.state().await, ServerState::Stopped, "a stop during the wait before a restart ends the supervisor");
        run.task.await.unwrap();
        assert_eq!(run.record.lock().unwrap().launches, 4);
    }

    #[tokio::test(start_paused = true)]
    async fn repeated_failures_end_in_failed_and_retry_starts_over() {
        let timing = Timing { give_up_after: 3, ..Timing::default() };
        let mut run = Harness::start(vec![Script::CannotStart("no such program"), Script::Dies("exit code 2"), Script::Dies("exit code 2"), Script::Healthy], timing);
        assert_eq!(run.state().await, starting(1));
        assert_eq!(run.state().await, ServerState::Restarting { attempt: 1, in_ms: 500, reason: "no such program".into() });
        assert_eq!(run.state().await, starting(2));
        assert!(matches!(run.state().await, ServerState::Restarting { attempt: 2, in_ms: 1000, .. }));
        assert_eq!(run.state().await, starting(3));
        assert_eq!(run.state().await, ServerState::Failed { reason: "It stopped before it answered (exit code 2).".into() });
        assert_eq!(run.record.lock().unwrap().launches, 3, "it tried no more");
        run.commands.send(Command::Retry).unwrap();
        assert_eq!(run.state().await, starting(1));
        assert!(matches!(run.state().await, ServerState::Ready { launch: 3, .. }));
        run.commands.send(Command::Stop).unwrap();
        assert_eq!(run.state().await, ServerState::Stopped);
        run.task.await.unwrap();
        // A stop while failed ends it as well.
        let mut run = Harness::start(vec![Script::CannotStart("a"), Script::CannotStart("b")], Timing { give_up_after: 2, ..Timing::default() });
        for _ in 0..3 {
            run.state().await;
        }
        assert!(matches!(run.state().await, ServerState::Failed { .. }));
        run.commands.send(Command::Stop).unwrap();
        assert_eq!(run.state().await, ServerState::Stopped);
    }

    #[tokio::test(start_paused = true)]
    async fn a_server_that_never_answers_is_ended_after_the_readiness_timeout_and_started_again() {
        let mut run = Harness::start(vec![Script::Silent, Script::Healthy], Timing::default());
        let (state, started) = run.next().await;
        assert_eq!(state, starting(1));
        let (restarting, gave_up) = run.next().await;
        assert_eq!(restarting, ServerState::Restarting { attempt: 1, in_ms: 500, reason: "It did not answer within 60 seconds.".into() });
        assert_eq!(gave_up - started, Duration::from_secs(60));
        assert_eq!(run.state().await, starting(2));
        assert!(matches!(run.state().await, ServerState::Ready { .. }));
        let record = run.record.lock().unwrap();
        assert_eq!((record.terminated, record.killed), (1, 0), "the silent one was asked to stop, and did");
    }

    #[tokio::test(start_paused = true)]
    async fn a_server_that_ignores_the_request_to_stop_is_ended_after_the_grace_period() {
        let mut run = Harness::start(vec![Script::Stubborn], Timing::default());
        assert_eq!(run.state().await, starting(1));
        assert!(matches!(run.state().await, ServerState::Ready { .. }));
        let asked = Instant::now();
        run.commands.send(Command::Stop).unwrap();
        let (state, stopped) = run.next().await;
        assert_eq!(state, ServerState::Stopped);
        assert_eq!(stopped - asked, Duration::from_secs(5));
        let record = run.record.lock().unwrap();
        assert_eq!((record.terminated, record.killed), (1, 1));
    }

    #[tokio::test(start_paused = true)]
    async fn a_restart_ends_the_running_server_and_starts_another_as_a_first_start() {
        let mut run = Harness::start(vec![Script::Healthy, Script::Dies("exit code 1"), Script::Healthy], Timing::default());
        assert_eq!(run.state().await, starting(1));
        assert!(matches!(run.state().await, ServerState::Ready { launch: 1, .. }));
        run.commands.send(Command::Restart).unwrap();
        // The first server is asked to stop, and the next one starts without a failure counted.
        assert_eq!(run.state().await, starting(1));
        assert!(matches!(run.state().await, ServerState::Restarting { attempt: 1, .. }), "the second one died");
        assert_eq!(run.state().await, starting(2));
        assert!(matches!(run.state().await, ServerState::Ready { launch: 3, .. }));
        // And again, from a server that answered.
        run.commands.send(Command::Restart).unwrap();
        assert_eq!(run.state().await, starting(1));
        assert!(matches!(run.state().await, ServerState::Ready { launch: 4, .. }));
        run.commands.send(Command::Stop).unwrap();
        assert_eq!(run.state().await, ServerState::Stopped);
        run.task.await.unwrap();
        let record = run.record.lock().unwrap();
        assert_eq!(record.launches, 4);
        assert_eq!(record.terminated, 3, "each server that was running when asked was asked to stop");
    }

    #[tokio::test(start_paused = true)]
    async fn a_server_that_names_the_address_it_answers_at_is_ready_there() {
        let record = Arc::new(Mutex::new(Record::default()));
        let (commands, command_receiver) = mpsc::unbounded_channel();
        let (sender, mut states) = mpsc::unbounded_channel();
        let launcher = FakeLauncher { scripts: vec![Script::Healthy].into(), record, answers: false, names: Some("http://127.0.0.1:4000".into()) };
        let task = tokio::spawn(run(launcher, Timing::default(), command_receiver, move |state| {
            let _ = sender.send(state);
        }));
        assert_eq!(states.recv().await.unwrap(), starting(1));
        assert!(matches!(states.recv().await.unwrap(), ServerState::Ready { address, .. } if address == "http://127.0.0.1:4000"));
        commands.send(Command::Stop).unwrap();
        assert_eq!(states.recv().await.unwrap(), ServerState::Stopped);
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_the_handle_stops_the_supervisor_like_a_stop() {
        let mut run = Harness::start(vec![Script::Healthy], Timing::default());
        run.state().await;
        run.state().await;
        let Harness { commands, mut states, task, .. } = run;
        drop(commands);
        assert_eq!(states.recv().await.unwrap().0, ServerState::Stopped);
        task.await.unwrap();
    }
}
