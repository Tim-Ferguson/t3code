//! Receiving the ChatGPT sign-in redirect on this computer, for an environment that is not on it: the desktop app
//! listens on the loopback port the sign-in names, opens the sign-in page, and hands the redirect's address back for the
//! environment to finish with. Port of `packages/shared/src/codexAuthCallback.ts` as the desktop main process uses it
//! (`receiveProviderAuthCallback`, `cancelProviderAuthCallback`).
//!
//! The credentials and the PKCE secret stay on the environment: only the redirect is read here, and only a redirect that
//! `t3_client::chatgpt::callback_url` accepts for this sign-in's `state`. Nothing here contacts OpenAI; the sign-in page is
//! opened by the caller's `open_browser`.
use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use t3_client::chatgpt::{self, AuthorizationRequest};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Notify,
};

/// A sign-in the person leaves alone for this long has expired.
pub const EXPIRY: Duration = Duration::from_secs(300);

const PAGE: &str = "<!doctype html><html><head><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><meta name=\"color-scheme\" content=\"light dark\"><title>T3 Code</title><style>body{font-family:system-ui;display:grid;place-items:center;min-height:90vh;margin:0}main{max-width:360px;padding:32px}h1{font-size:24px}p{line-height:1.6;opacity:.7}</style></head><body><main><h1>Return to T3 Code</h1><p>Your sign-in response has been received. T3 Code is finishing the connection. You can close this tab.</p></main></body></html>";

fn listeners() -> &'static Mutex<HashMap<String, Arc<Notify>>> {
    static LISTENERS: OnceLock<Mutex<HashMap<String, Arc<Notify>>>> = OnceLock::new();
    LISTENERS.get_or_init(Default::default)
}

/// Stops the listener of a sign-in, if one is waiting (`cancelCodexAuthCallback`). An address that is not a sign-in stops nothing.
pub fn cancel(authorization_url: &str) {
    if let Ok(request) = chatgpt::authorization_request(authorization_url) {
        if let Some(stop) = listeners().lock().unwrap().get(&request.state) {
            stop.notify_one();
        }
    }
}

/// Removes the listener's entry however the wait ends.
struct Registered(String);
impl Drop for Registered {
    fn drop(&mut self) {
        listeners().lock().unwrap().remove(&self.0);
    }
}

fn response(status: &str, body: &str, content_type: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\ncache-control: no-store\r\nreferrer-policy: no-referrer\r\nx-content-type-options: nosniff\r\ncontent-security-policy: default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Reads one request from a connection and answers it; `Some` is the redirect it carried, accepted for this sign-in.
async fn serve(mut stream: TcpStream, request: &AuthorizationRequest) -> Option<String> {
    let mut head = Vec::new();
    let mut chunk = [0u8; 2048];
    // Only the request line is needed; a request that does not finish its head in 16 KiB is not a browser redirect.
    while !head.windows(2).any(|pair| pair == b"\r\n") && head.len() < 16 * 1024 {
        match tokio::time::timeout(Duration::from_secs(5), stream.read(&mut chunk)).await {
            Ok(Ok(read)) if read > 0 => head.extend_from_slice(&chunk[..read]),
            _ => break,
        }
    }
    let line = String::from_utf8_lossy(&head).lines().next().unwrap_or_default().to_owned();
    let mut parts = line.split(' ');
    let accepted = match (parts.next(), parts.next()) {
        (Some("GET"), Some(target)) if target.starts_with('/') => ::url::Url::parse(&request.redirect_uri)
            .ok()
            .and_then(|base| base.join(target).ok())
            .and_then(|joined| chatgpt::callback_url(joined.as_str(), &request.redirect_uri, &request.state).ok())
            .map(|url| url.to_string()),
        _ => None,
    };
    let answer = match &accepted {
        Some(_) => response("200 OK", PAGE, "text/html; charset=utf-8"),
        None => response("400 Bad Request", "This response does not belong to the active sign-in.", "text/plain; charset=utf-8"),
    };
    let _ = stream.write_all(answer.as_bytes()).await;
    let _ = stream.shutdown().await;
    accepted
}

/// Listens for the redirect of the sign-in at `authorization_url`, opens that address with `open_browser`, and returns the
/// redirect's address. Fails when the address is not a sign-in this app opens, when the port is in use, when the browser could not
/// be opened, when `cancel` is called, and when `expiry` passes. A second wait for the same sign-in is refused.
pub async fn receive<F, Fut>(authorization_url: &str, open_browser: F, expiry: Duration) -> Result<String, String>
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = bool>,
{
    let request = chatgpt::authorization_request(authorization_url)?;
    let stop = Arc::new(Notify::new());
    {
        let mut waiting = listeners().lock().unwrap();
        if waiting.contains_key(&request.state) {
            return Err("This sign-in is already open on this computer.".into());
        }
        waiting.insert(request.state.clone(), stop.clone());
    }
    let _registered = Registered(request.state.clone());
    let port: u16 = ::url::Url::parse(&request.redirect_uri).ok().and_then(|url| url.port()).ok_or("Invalid ChatGPT sign-in request.")?;
    let listener = TcpListener::bind(("127.0.0.1", port)).await.map_err(|_| "The ChatGPT callback port is in use on this computer. Close the other sign-in and try again, or paste the redirect URL in T3 Code.".to_owned())?;
    if !open_browser(request.authorization_url.clone()).await {
        return Err("Could not open your sign-in browser.".into());
    }
    let wait = async {
        loop {
            let (stream, _) = listener.accept().await.map_err(|_| "The sign-in listener stopped. Try again.".to_owned())?;
            if let Some(url) = serve(stream, &request).await {
                return Ok(url);
            }
        }
    };
    tokio::select! {
        outcome = wait => outcome,
        _ = stop.notified() => Err("Sign-in cancelled on this computer.".into()),
        _ = tokio::time::sleep(expiry) => Err("Sign-in expired. Try again.".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU16, Ordering};

    const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
    /// Each test names its own state and port, so the process-wide registry never sees two alike.
    fn sign_in(port: u16, state: &str) -> String {
        format!("https://auth.openai.com/api/accounts/authorize?response_type=code&client_id=dynamic_agent_client&redirect_uri=http%3A%2F%2F127.0.0.1%3A{port}%2Fauth%2Fcallback&state={state}&code_challenge={CHALLENGE}&code_challenge_method=S256")
    }
    /// A port nothing listens on: bound and released.
    fn free_port() -> u16 {
        static LAST: AtomicU16 = AtomicU16::new(0);
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        LAST.store(port, Ordering::Relaxed);
        port
    }
    async fn get(port: u16, target: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(format!("GET {target} HTTP/1.1\r\nhost: 127.0.0.1\r\n\r\n").as_bytes()).await.unwrap();
        let mut body = String::new();
        stream.read_to_string(&mut body).await.unwrap();
        body
    }

    #[tokio::test]
    async fn the_redirect_of_this_sign_in_is_returned_and_one_of_another_is_refused() {
        let (port, state) = (free_port(), "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1");
        let (opened, opened_rx) = tokio::sync::oneshot::channel();
        let address = sign_in(port, state);
        let waiting = tokio::spawn({
            let address = address.clone();
            async move {
                receive(&address, |url| async move { let _ = opened.send(url); true }, EXPIRY).await
            }
        });
        // The sign-in page is the one the environment named; the listener is up by the time it is opened.
        assert_eq!(opened_rx.await.unwrap(), address);
        assert!(get(port, &format!("/auth/callback?code=x&state=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb2")).await.starts_with("HTTP/1.1 400"), "another sign-in's state");
        assert!(get(port, "/elsewhere?code=x&state=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1").await.starts_with("HTTP/1.1 400"), "another path");
        let page = get(port, &format!("/auth/callback?code=the-code&state={state}")).await;
        assert!(page.starts_with("HTTP/1.1 200") && page.contains("Return to T3 Code") && page.contains("no-store"), "{page}");
        assert_eq!(waiting.await.unwrap(), Ok(format!("http://127.0.0.1:{port}/auth/callback?code=the-code&state={state}")));
        assert!(TcpListener::bind(("127.0.0.1", port)).await.is_ok(), "the port is released when the wait ends");
    }

    #[tokio::test]
    async fn an_address_that_is_not_a_sign_in_opens_nothing_and_listens_on_nothing() {
        let opened = std::sync::atomic::AtomicBool::new(false);
        let outcome = receive("https://evil.test/api/accounts/authorize", |_| async { opened.store(true, Ordering::Relaxed); true }, EXPIRY).await;
        assert_eq!(outcome, Err("Invalid ChatGPT sign-in request.".into()));
        assert!(!opened.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn a_port_in_use_a_browser_that_will_not_open_a_second_wait_a_cancel_and_the_clock_each_end_the_wait_with_a_reason() {
        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let in_use = receive(&sign_in(taken.local_addr().unwrap().port(), "ccccccccccccccccccccccccccccccc3"), |_| async { true }, EXPIRY).await;
        assert!(in_use.unwrap_err().contains("callback port is in use"));

        let no_browser = receive(&sign_in(free_port(), "ddddddddddddddddddddddddddddddd4"), |_| async { false }, EXPIRY).await;
        assert_eq!(no_browser, Err("Could not open your sign-in browser.".into()));
        assert!(!listeners().lock().unwrap().contains_key("ddddddddddddddddddddddddddddddd4"), "a failed wait leaves nothing registered");

        let address = sign_in(free_port(), "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeee5");
        let (opened, opened_rx) = tokio::sync::oneshot::channel();
        let first = tokio::spawn({
            let address = address.clone();
            async move { receive(&address, |_| async move { let _ = opened.send(()); true }, EXPIRY).await }
        });
        opened_rx.await.unwrap();
        assert_eq!(receive(&address, |_| async { true }, EXPIRY).await, Err("This sign-in is already open on this computer.".into()));
        cancel(&address);
        assert_eq!(first.await.unwrap(), Err("Sign-in cancelled on this computer.".into()));
        cancel("not an address");

        let expired = receive(&sign_in(free_port(), "fffffffffffffffffffffffffffffff6"), |_| async { true }, Duration::from_millis(1)).await;
        assert_eq!(expired, Err("Sign-in expired. Try again.".into()));
    }
}
