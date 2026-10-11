//! `provider.codex.auth-callback.subscribe` over a real socket: the environment listens on the loopback port a ChatGPT sign-in redirects to,
//! reports it ready, answers only the callback of its state, hands the callback URL to the client and frees the port. OpenAI is never
//! contacted: the authorize URL is only checked, and the "browser" is a plain loopback request.
mod rpc_support;
use rpc_support::*;
use serde_json::{Value, json};
use t3_server::test_support::HANG_BOUND;

const STATE: &str = "abcdefghijklmnopqrstuvwxyz012345";
/// A second test runs beside the first, and a state is open once per process.
const OTHER_STATE: &str = "zyxwvutsrqponmlkjihgfedcba543210";

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn authorize(port: u16, state: &str) -> String {
    let mut url = url::Url::parse("https://auth.openai.com/api/accounts/authorize").unwrap();
    url.query_pairs_mut()
        .append_pair("client_id", "dynamic_agent_client")
        .append_pair("redirect_uri", &format!("http://127.0.0.1:{port}/auth/callback"))
        .append_pair("response_type", "code")
        .append_pair("code_challenge_method", "S256")
        .append_pair("code_challenge", &"A".repeat(43))
        .append_pair("state", state);
    url.to_string()
}

fn subscription(authorization_url: &str, return_url: &str) -> Value {
    json!({"instanceId":"codex","environmentId":"environment-1","flowId":"flow-1","authorizationUrl":authorization_url,"returnUrl":return_url})
}

fn manager(h: &Harness) -> String {
    h.auth.issue_session("manager", "bearer-access-token", vec![t3_contracts::AuthEnvironmentScope::OrchestrationRead, t3_contracts::AuthEnvironmentScope::ProvidersManage], json!({"deviceType":"unknown"}), chrono::Utc::now(), chrono::Duration::hours(1)).unwrap().1
}

/// The next chunk's values, acknowledged.
async fn chunk(socket: &mut Socket, id: u64) -> Vec<Value> {
    loop {
        let message = tokio::time::timeout(HANG_BOUND, next(socket)).await.unwrap();
        if message["_tag"] == "Chunk" {
            send(socket, json!({"_tag":"Ack","requestId":id})).await;
            return message["values"].as_array().unwrap().clone();
        }
        assert_ne!(message["_tag"], "Exit", "{message}");
    }
}
async fn exit(socket: &mut Socket) -> Value {
    loop {
        let message = tokio::time::timeout(HANG_BOUND, next(socket)).await.unwrap();
        if message["_tag"] == "Exit" {
            return message["exit"].clone();
        }
    }
}

async fn get(url: &str) -> reqwest::Response {
    reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap().get(url).send().await.unwrap()
}

#[tokio::test]
async fn the_environment_receives_the_callback_for_its_sign_in_and_nothing_else() {
    let h = harness().await;
    let mut socket = h.connect(&manager(&h)).await;
    let port = free_port();
    let returned = "http://localhost:5173/settings/providers?instanceId=codex&junk=1";
    send(&mut socket, json!({"_tag":"Request","id":1,"tag":"provider.codex.auth-callback.subscribe","payload":subscription(&authorize(port, STATE), returned),"headers":[]})).await;
    assert_eq!(chunk(&mut socket, 1).await, vec![json!({"phase":"ready"})]);

    // Only a GET of the registered path with this sign-in's state and a code or an error is the callback.
    let base = format!("http://127.0.0.1:{port}");
    assert_eq!(get(&format!("{base}/other?code=c&state={STATE}")).await.status(), 400);
    assert_eq!(get(&format!("{base}/auth/callback?code=c&state=wrong")).await.status(), 400);
    assert_eq!(reqwest::Client::new().post(format!("{base}/auth/callback?code=c&state={STATE}")).send().await.unwrap().status(), 400);
    let answered = get(&format!("{base}/auth/callback?code=the-code&state={STATE}")).await;
    assert_eq!(answered.status(), 303);
    assert_eq!(answered.headers()["location"], "http://localhost:5173/settings/providers?instanceId=codex", "the browser returns to T3 Code without the junk");
    assert_eq!(answered.headers()["cache-control"], "no-store");
    assert_eq!(answered.headers()["referrer-policy"], "no-referrer");
    assert_eq!(chunk(&mut socket, 1).await, vec![json!({"phase":"finished","callbackUrl":format!("{base}/auth/callback?code=the-code&state={STATE}")})]);
    assert_eq!(success(&exit(&mut socket).await), &Value::Null);
    // The port is free again, and the same sign-in can be listened for anew.
    tokio::time::timeout(HANG_BOUND, async { while tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() { tokio::task::yield_now().await; } }).await.unwrap();
    h.finish().await;
}

#[tokio::test]
async fn a_request_the_environment_cannot_honour_is_refused_in_the_originals_words() {
    let h = harness().await;
    let mut socket = h.connect(&manager(&h)).await;
    let port = free_port();
    let ask = |id: u64, authorization_url: String, returned: &str| json!({"_tag":"Request","id":id,"tag":"provider.codex.auth-callback.subscribe","payload":subscription(&authorization_url, returned),"headers":[]});
    let detail = |exit: &Value| {
        let error = failure(exit);
        assert_eq!((error["_tag"].as_str(), error["operation"].as_str(), error["instanceId"].as_str()), (Some("ProviderSetupError"), Some("callback"), Some("codex")));
        error["detail"].as_str().unwrap().to_owned()
    };
    // Not OpenAI's authorize endpoint.
    send(&mut socket, ask(1, authorize(port, OTHER_STATE).replace("auth.openai.com", "evil.example"), "http://localhost:5173/welcome")).await;
    assert_eq!(detail(&exit(&mut socket).await), "Invalid ChatGPT sign-in request.");
    // A return address that is not local.
    send(&mut socket, ask(2, authorize(port, OTHER_STATE), "https://app.t3.codes/welcome")).await;
    assert_eq!(detail(&exit(&mut socket).await), "The local sign-in receiver needs a local T3 Code return address.");
    // A port something else holds.
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    send(&mut socket, ask(3, authorize(held.local_addr().unwrap().port(), OTHER_STATE), "http://localhost:5173/welcome")).await;
    let in_use = detail(&exit(&mut socket).await);
    assert!(in_use.starts_with("The ChatGPT callback port is in use on this computer."), "{in_use}");
    // The same sign-in twice at once.
    send(&mut socket, ask(4, authorize(port, OTHER_STATE), "http://localhost:5173/welcome")).await;
    assert_eq!(chunk(&mut socket, 4).await, vec![json!({"phase":"ready"})]);
    send(&mut socket, ask(5, authorize(free_port(), OTHER_STATE), "http://localhost:5173/welcome")).await;
    assert_eq!(detail(&exit(&mut socket).await), "This sign-in is already open on this computer.");
    // Leaving cancels the first and frees its port.
    send(&mut socket, json!({"_tag":"Interrupt","requestId":4})).await;
    tokio::time::timeout(HANG_BOUND, async { while tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() { tokio::task::yield_now().await; } }).await.unwrap();
    h.finish().await;
}
