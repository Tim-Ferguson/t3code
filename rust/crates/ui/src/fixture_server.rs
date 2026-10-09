//! The one way a UI test serves a real in-process server, and the one place it is torn down.
//!
//! A failed assertion unwinds the test body instead of running its success path, and the
//! runtime then waits at drop for every blocking-pool task. The server's settings owner is one:
//! it waits for its command channel to close, but the process-wide git status source keeps a clone
//! of the last settings service it served, so the channel never closes. A fixture that is only
//! shut down on its success path therefore turns a failed assertion into a hang. `Server` owns
//! the HTTP task and a shutdown for every service the served `ApiState` carries, plus any a
//! fixture adds with `also`. It stops them when awaited (success path) and when dropped
//! (unwinding), so no fixture can forget.
use std::{future::Future, pin::Pin};
use t3_server::transport::ApiState;

type Stop = Pin<Box<dyn Future<Output = ()> + Send>>;

pub(crate) struct Server {
    handle: Option<tokio::task::JoinHandle<()>>,
    stops: Vec<Stop>,
}
impl Server {
    /// A guard for services the fixture serves itself.
    pub(crate) fn guard() -> Self {
        Self { handle: None, stops: Vec::new() }
    }
    pub(crate) fn abort(&self) {
        if let Some(handle) = &self.handle {
            handle.abort();
        }
    }
    /// A service the fixture started that `ApiState` does not carry.
    pub(crate) fn also<F: Future<Output = ()> + Send + 'static>(&mut self, stop: F) {
        self.stops.push(Box::pin(stop));
    }
    /// Stops the server and then every service, in the runtime that is still running.
    pub(crate) async fn stop(mut self) {
        self.abort();
        if let Some(handle) = self.handle.take() {
            let _ = handle.await;
        }
        for stop in std::mem::take(&mut self.stops) {
            stop.await;
        }
    }
}
impl std::future::IntoFuture for Server {
    type Output = ();
    type IntoFuture = Pin<Box<dyn Future<Output = ()>>>;
    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.stop())
    }
}
impl Drop for Server {
    /// Reached only unwinding, or when a test never stopped its server. The runtime is no longer
    /// running, and is about to wait for blocking-pool tasks, so each shutdown goes to a thread of
    /// its own with a private runtime: one that cannot finish without the dead runtime must not
    /// starve the rest. The threads are detached; a shutdown that ends only once the dead
    /// runtime has dropped its tasks finishes when it does.
    fn drop(&mut self) {
        self.abort();
        for stop in std::mem::take(&mut self.stops) {
            let _ = std::thread::Builder::new().name("fixture-teardown".into()).spawn(move || {
                if let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    runtime.block_on(stop);
                }
            });
        }
    }
}

/// Serves the API, and stops it, with every service it carries, when awaited or dropped.
pub(crate) async fn serve(api: ApiState) -> (String, Server) {
    // Every server of this test binary asks the same process-wide host for source control tools;
    // it is the fake one, in place before any of them can use the real one.
    #[cfg(unix)]
    let _ = crate::runtime::source_control_tests::world();
    let mut stops: Vec<Stop> = Vec::new();
    macro_rules! stop {
        ($($service:ident),*) => {$(
            if let Some(service) = api.$service.clone() {
                stops.push(Box::pin(async move { service.shutdown().await }));
            }
        )*};
    }
    stop!(execution, terminals, discovery, resource_telemetry, host_resources, background, device_hosts, devices, provider_auth);
    // Last: the settings owner is what the runtime waits for, and the others may read it.
    stop!(settings);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        axum::serve(listener, t3_server::transport::router(api)).await.unwrap();
    });
    (address, Server { handle: Some(handle), stops })
}
