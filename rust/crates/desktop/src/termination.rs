//! The app being told to end by the system: SIGINT (an interrupt in the terminal it was started from) and SIGTERM (`kill`, a
//! service manager, a log out). The original quits the way a person's quit does (`quitFromSignal` of `app/DesktopLifecycle.ts`,
//! registered everywhere but Windows): the first signal begins the shutdown, which closes the control socket and stops the server
//! it started, and a signal that comes while that runs changes nothing. Without this a terminated app leaves its control socket
//! file behind and its server running.
//!
//! [`on_termination`] is the real thing, on a thread of its own so that it works whatever the window is doing (a window that is
//! hidden or covered does not run the page). [`serve`] is the same over messages, which is how a test says a signal came.
use std::io;
use tokio::sync::mpsc;

/// Which signal ended the app (`signal` of the original's log line).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Termination {
    Interrupt,
    Terminate,
}

impl Termination {
    pub fn name(self) -> &'static str {
        match self {
            Self::Interrupt => "SIGINT",
            Self::Terminate => "SIGTERM",
        }
    }
}

/// `quitFromSignal`: `quit` is called with the first signal that comes, once; later ones are read and dropped. Returns when `signals`
/// is closed.
pub async fn serve(mut signals: mpsc::UnboundedReceiver<Termination>, quit: impl FnOnce(Termination)) {
    let mut quit = Some(quit);
    while let Some(signal) = signals.recv().await {
        if let Some(quit) = quit.take() {
            quit(signal);
        }
    }
}

/// The system's signals as messages. Must be called inside a Tokio runtime. None on a system that has no such signals.
#[cfg(unix)]
fn system_signals() -> io::Result<mpsc::UnboundedReceiver<Termination>> {
    use tokio::signal::unix::{SignalKind, signal};
    let (sender, receiver) = mpsc::unbounded_channel();
    for (kind, termination) in [(SignalKind::interrupt(), Termination::Interrupt), (SignalKind::terminate(), Termination::Terminate)] {
        // Registered here, before anything can wait on it, so that a signal that comes at once is held for the first read.
        let mut stream = signal(kind)?;
        let sender = sender.clone();
        tokio::spawn(async move {
            while stream.recv().await.is_some() {
                if sender.send(termination).is_err() {
                    return;
                }
            }
        });
    }
    Ok(receiver)
}

/// Calls `quit` on a thread of its own when the process gets SIGINT or SIGTERM. From this call on those signals no longer end the
/// process by themselves: `quit` is what ends it. Windows has neither signal, and the original registers none there.
pub fn on_termination(quit: impl FnOnce(Termination) + Send + 'static) -> io::Result<()> {
    #[cfg(unix)]
    {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        // Registered before this returns, on the runtime the thread will run.
        let signals = runtime.block_on(async { system_signals() })?;
        std::thread::Builder::new().name("t3-termination".into()).spawn(move || runtime.block_on(serve(signals, quit)))?;
    }
    #[cfg(not(unix))]
    let _ = quit;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn the_first_signal_quits_once_and_the_ones_after_it_change_nothing() {
        let (sender, receiver) = mpsc::unbounded_channel();
        let quits = Arc::new(Mutex::new(Vec::new()));
        sender.send(Termination::Terminate).unwrap();
        sender.send(Termination::Interrupt).unwrap();
        drop(sender);
        let record = quits.clone();
        serve(receiver, move |signal| record.lock().unwrap().push(signal)).await;
        assert_eq!(*quits.lock().unwrap(), [Termination::Terminate]);
    }

    #[tokio::test]
    async fn no_signal_is_no_quit() {
        let (sender, receiver) = mpsc::unbounded_channel();
        drop(sender);
        serve(receiver, |signal| panic!("quit on {signal:?}")).await;
    }

    /// The real signals, sent to a child this test starts and to nothing else. The child is this test program run again for one test
    /// that, when told it is the child, waits to be signalled and says which signal it was.
    #[cfg(unix)]
    mod real {
        use super::*;
        use std::io::{BufRead, BufReader};
        use std::process::{Command, Stdio};

        const CHILD: &str = "T3_TERMINATION_TEST_CHILD";

        /// The child's half: a no-op in the test run itself.
        #[test]
        fn child_waits_for_a_signal() {
            if std::env::var_os(CHILD).is_none() {
                return;
            }
            on_termination(|signal| {
                println!("quit {}", signal.name());
                std::process::exit(7);
            })
            .unwrap();
            // The harness has begun the line with the test's name.
            println!("ready");
            loop {
                std::thread::park();
            }
        }

        fn signalled(signal: libc::c_int) -> (Option<i32>, Vec<String>) {
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "termination::tests::real::child_waits_for_a_signal", "--nocapture", "--test-threads=1"])
                .env(CHILD, "1")
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let pid = child.id() as libc::pid_t;
            let mut lines = BufReader::new(child.stdout.take().unwrap()).lines().map(Result::unwrap);
            // The child has begun to listen when it says so; the signal is sent to the pid this test started and no other.
            assert!(lines.by_ref().any(|line| line.ends_with("ready")));
            assert_eq!(unsafe { libc::kill(pid, signal) }, 0);
            let rest: Vec<String> = lines.collect();
            (child.wait().unwrap().code(), rest)
        }

        #[test]
        fn a_terminated_process_quits_the_way_it_was_told_to_and_is_not_ended_by_the_signal() {
            let (code, said) = signalled(libc::SIGTERM);
            assert_eq!((code, said.iter().any(|line| line == "quit SIGTERM")), (Some(7), true), "{said:?}");
        }

        #[test]
        fn an_interrupted_process_quits_too() {
            let (code, said) = signalled(libc::SIGINT);
            assert_eq!((code, said.iter().any(|line| line == "quit SIGINT")), (Some(7), true), "{said:?}");
        }
    }
}
