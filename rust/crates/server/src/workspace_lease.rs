//! Coordinates checkout removal and startup across threads using the same resolved directory
//! (`workspace/workspaceLease.ts`). Storage cleanup removes a worktree under the lease of its path, and a
//! terminal opens or restarts under the same lease, so a terminal cannot start in a checkout while it is being
//! removed, and a removal re-checks for terminals only once it holds the lease.
use std::{
    collections::HashMap,
    future::Future,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
};

struct Lease {
    gate: Arc<tokio::sync::Mutex<()>>,
    users: usize,
}

fn leases() -> &'static Mutex<HashMap<PathBuf, Lease>> {
    static LEASES: OnceLock<Mutex<HashMap<PathBuf, Lease>>> = OnceLock::new();
    LEASES.get_or_init(Mutex::default)
}

/// Releases the user count even when the future is dropped mid-wait.
struct Registration(PathBuf);
impl Drop for Registration {
    fn drop(&mut self) {
        let mut leases = leases().lock().unwrap();
        if let Some(lease) = leases.get_mut(&self.0) {
            lease.users -= 1;
            if lease.users == 0 {
                leases.remove(&self.0);
            }
        }
    }
}

/// `path.resolve`: absolute (against the working directory) and lexically normalised, symbolic links untouched.
pub fn resolve(path: &str) -> PathBuf {
    let joined = if std::path::Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// Runs `work` while holding the lease of `cwd`; callers on the same path run one at a time.
pub async fn with_workspace_lease<T>(cwd: impl Into<PathBuf>, work: impl Future<Output = T>) -> T {
    let cwd = cwd.into();
    let gate = {
        let mut leases = leases().lock().unwrap();
        let lease = leases.entry(cwd.clone()).or_insert_with(|| Lease { gate: Arc::new(tokio::sync::Mutex::new(())), users: 0 });
        lease.users += 1;
        lease.gate.clone()
    };
    let _registration = Registration(cwd);
    let _permit = gate.lock().await;
    work.await
}

/// For tests: whether any lease is still registered for `cwd`.
#[cfg(test)]
pub(crate) fn registered(cwd: &std::path::Path) -> bool {
    leases().lock().unwrap().contains_key(cwd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn one_path_runs_one_holder_at_a_time_and_the_registry_empties() {
        let path = PathBuf::from("/t3code-rust-port-test-lease");
        let (entered, mut inside) = tokio::sync::mpsc::unbounded_channel();
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let first = tokio::spawn(with_workspace_lease(path.clone(), async move {
            entered.send(()).unwrap();
            released.await.unwrap();
        }));
        inside.recv().await.unwrap();
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let second = tokio::spawn({
            let (path, ran) = (path.clone(), ran.clone());
            async move { with_workspace_lease(path, async move { ran.store(true, std::sync::atomic::Ordering::SeqCst) }).await }
        });
        // A holder on another path is not held up by the first.
        with_workspace_lease("/t3code-rust-port-test-lease-other", async {}).await;
        assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
        release.send(()).unwrap();
        first.await.unwrap();
        second.await.unwrap();
        assert!(ran.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!registered(&path));
    }
}
