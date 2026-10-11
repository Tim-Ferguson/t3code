//! `provider.codex.auth-callback.subscribe` (`provider/CodexAuthCallback.ts`, `packages/shared/src/codexAuthCallback.ts`): a connected local
//! environment listens on the loopback port a ChatGPT sign-in is registered to redirect to, and hands the callback URL to the client, which
//! passes it on to the environment that owns the tokens (credentials and PKCE stay there). The listener serves one sign-in, answers only
//! the callback of its state, and gives up after five minutes.
use crate::codex_auth_handoff::{codex_authorization_request, codex_callback_url, is_loopback_host, provider_auth_return_url};
use axum::{
    Router,
    body::Body,
    extract::State,
    http::{HeaderValue, Method, Request, StatusCode, header},
    response::Response,
};
use std::{
    collections::HashSet,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot, watch};

/// How long a sign-in may take before the listener gives up.
pub const EXPIRY: Duration = Duration::from_secs(300);

/// What the stream carries: the listener is up, then the callback arrives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Ready,
    Finished { callback_url: String },
}

/// A failure, in the words the original uses (`ProviderSetupError.detail`).
pub type Failure = String;

fn active() -> &'static Mutex<HashSet<String>> {
    static ACTIVE: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    ACTIVE.get_or_init(Default::default)
}

struct Held(String);
impl Drop for Held {
    fn drop(&mut self) {
        active().lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(&self.0);
    }
}

#[derive(Clone)]
struct Listener {
    redirect_uri: String,
    state: String,
    destination: String,
    callback: Arc<Mutex<Option<oneshot::Sender<String>>>>,
}

async fn answer(State(listener): State<Listener>, request: Request<Body>) -> Response {
    let refuse = || Response::builder().status(StatusCode::BAD_REQUEST).body(Body::from("This response does not belong to the active sign-in.")).unwrap();
    if request.method() != Method::GET {
        return refuse();
    }
    // The request line's target, read against the registered redirect: only that path and that origin pass.
    let base = url::Url::parse(&listener.redirect_uri).expect("the redirect was checked");
    let Ok(target) = base.join(&request.uri().path_and_query().map_or_else(|| "/".to_owned(), ToString::to_string)) else { return refuse() };
    let Ok(callback) = codex_callback_url(target.as_str(), &listener.redirect_uri, &listener.state) else { return refuse() };
    // The first valid response is the sign-in's; a second one finds the sender spent.
    let Some(sender) = listener.callback.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take() else { return refuse() };
    let _ = sender.send(callback.to_string());
    let mut response = Response::builder().status(StatusCode::SEE_OTHER).body(Body::empty()).unwrap();
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    if let Ok(location) = HeaderValue::from_str(&listener.destination) {
        headers.insert(header::LOCATION, location);
    }
    response
}

/// `subscribeCodexAuthCallback`: validates the request, listens, and reports `Ready` once the port is bound and `Finished` with the
/// callback URL. Dropping the receiver cancels the sign-in and frees the port. `expiry` is [`EXPIRY`] outside tests.
pub fn subscribe(authorization_url: &str, return_url: &str, expiry: Duration) -> mpsc::Receiver<Result<Event, Failure>> {
    let (events, receiver) = mpsc::channel(2);
    let (authorization_url, return_url) = (authorization_url.to_owned(), return_url.to_owned());
    tokio::spawn(async move {
        let outcome = async {
            let request = codex_authorization_request(&authorization_url).map_err(str::to_owned)?;
            let destination = provider_auth_return_url(Some(&return_url)).filter(|destination| url::Url::parse(destination).ok().and_then(|url| url.host_str().map(is_loopback_host)).unwrap_or(false)).ok_or("The local sign-in receiver needs a local T3 Code return address.")?;
            if !active().lock().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(request.state.clone()) {
                return Err("This sign-in is already open on this computer.".to_owned());
            }
            let _held = Held(request.state.clone());
            let port = url::Url::parse(&request.redirect_uri).ok().and_then(|url| url.port()).ok_or("Invalid ChatGPT sign-in request.")?;
            let socket = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.map_err(|_| "The ChatGPT callback port is in use on this computer. Close the other sign-in and try again, or paste the redirect URL in T3 Code.".to_owned())?;
            let (sender, received) = oneshot::channel();
            let listener = Listener { redirect_uri: request.redirect_uri.clone(), state: request.state.clone(), destination, callback: Arc::new(Mutex::new(Some(sender))) };
            let (stop, stopped) = watch::channel(false);
            let server = tokio::spawn({
                let mut stopped = stopped.clone();
                axum::serve(socket, Router::new().fallback(answer).with_state(listener)).with_graceful_shutdown(async move {
                    let _ = stopped.wait_for(|stop| *stop).await;
                })
                .into_future()
            });
            struct Stop(watch::Sender<bool>, tokio::task::JoinHandle<Result<(), std::io::Error>>);
            impl Drop for Stop {
                fn drop(&mut self) {
                    self.0.send_replace(true);
                    // A connection that is still open must not keep the port.
                    self.1.abort();
                }
            }
            let _stop = Stop(stop, server);
            if events.send(Ok(Event::Ready)).await.is_err() {
                return Err("Sign-in cancelled on this computer.".to_owned());
            }
            tokio::select! {
                callback = received => callback.map(|callback_url| Event::Finished { callback_url }).map_err(|_| "Sign-in cancelled on this computer.".to_owned()),
                () = tokio::time::sleep(expiry) => Err("Sign-in expired. Try again.".to_owned()),
                () = events.closed() => Err("Sign-in cancelled on this computer.".to_owned()),
            }
        }
        .await;
        let _ = events.send(outcome).await;
    });
    receiver
}
