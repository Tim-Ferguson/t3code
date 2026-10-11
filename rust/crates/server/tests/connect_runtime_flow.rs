//! What T3 Connect does while the server runs and when it stops: startup with a stored link, the held-webhook wake, what shutdown
//! releases and when, the hourly sweep of replay markers, the webhook delivery proof on the existing route, Tailscale Serve, and
//! the agent-activity publisher driven by committed events. Time that matters (debounces, confirmation, retries) is tokio's paused
//! clock; the relay is in process (`Recorder`), so nothing races a socket.
mod connect_support;
mod rpc_support;

use chrono::Utc;
use connect_support::*;
use rpc_support::{rpc, success};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use t3_remote::{
    jwt::{KeyPair, RELAY_ACTIVITY_PUBLISH_TYP, RELAY_HOOK_DELIVERY_HEADER, RELAY_HOOK_DELIVERY_TYP, VerifyOptions, sign_relay_jwt, verify_relay_jwt},
    runtime::LinkPhase,
    secrets::*,
    types::{ConfirmedOrigin, ManagedEndpointOrigin, RuntimeConfig},
};

fn no_relay(_: &RecordedRequest) -> (u16, Value) {
    (404, json!({}))
}

async fn started(plane: Arc<Recorder>, extra: &[(&str, String)]) -> Server {
    started_with_clock(plane, extra, None).await
}

async fn started_with_clock(plane: Arc<Recorder>, extra: &[(&str, String)], clock: Option<Arc<dyn t3_remote::clock::Clock>>) -> Server {
    let mut options = Options { start: false, plane: Some(plane), clock, ..Options::new(no_relay) };
    for (name, value) in extra {
        options = options.secret(name, value.clone());
    }
    let server = start(options).await;
    for (name, value) in connect_support::linked(&server.relay.url, &server.relay_keys) {
        server.connect.secrets.set(&name, value.as_bytes()).unwrap();
    }
    server
}

/// The secrets of a CLI-managed link whose tunnel the relay confirmed for this server's own port.
fn managed(server: &Server) {
    let config = RuntimeConfig::cloudflare("connector-1", Some("tunnel-1"));
    let confirmed = ConfirmedOrigin { config: config.clone(), origin: ManagedEndpointOrigin::new("127.0.0.1", server.address.port()) };
    for (name, value) in [
        (CLOUD_ENDPOINT_RUNTIME_CONFIG, serde_json::to_string(&config).unwrap()),
        (CLOUD_ENDPOINT_CONFIRMED_ORIGIN, serde_json::to_string(&confirmed).unwrap()),
        (CLOUD_CLI_DESIRED_LINK_SECRET, "managed".to_owned()),
    ] {
        server.connect.secrets.set(name, value.as_bytes()).unwrap();
    }
    server.sign_in_cli();
}

#[tokio::test(start_paused = true)]
async fn a_stored_link_starts_its_confirmed_tunnel_and_each_connection_wakes_held_webhooks() {
    let plane = Recorder::new();
    let server = started(plane.clone(), &[(HOLD_WEBHOOKS_WHILE_OFFLINE_SECRET, "true".into())]).await;
    managed(&server);
    server.connect.start();
    // The connector the relay confirmed for this origin starts without asking anyone.
    server.spawner.wait_for_spawns(1).await;
    assert_eq!(server.spawner.last().token, "connector-1");
    let mut status = server.connect.services.status();
    within(async {
        while status.borrow_and_update().phase != LinkPhase::EndpointStarting {
            status.changed().await.unwrap();
        }
    })
    .await;
    // Once the tunnel carries traffic, the relay is told to deliver what it held while this server was away; the CLI's sign-in is
    // not used for it, the environment credential is.
    server.spawner.last().say("2026-01-01T00:00:00Z INF Registered tunnel connection connIndex=0");
    plane.wait_for("wake:environment-credential", 1).await;
    within(async {
        while status.borrow_and_update().phase != LinkPhase::EndpointUp {
            status.changed().await.unwrap();
        }
    })
    .await;
    server.finish().await;
}

#[tokio::test(start_paused = true)]
async fn shutdown_releases_a_tunnel_the_cli_asked_for_and_forgets_the_dead_token() {
    let plane = Recorder::new();
    let server = started(plane.clone(), &[]).await;
    managed(&server);
    server.connect.start();
    server.spawner.wait_for_spawns(1).await;
    let server = server.finish().await;
    // The connector stops before the relay deletes the tunnel it serves, with the CLI's authorization.
    assert!(!server.spawner.last().is_running());
    assert_eq!(plane.count("release:cli-access"), 1);
    assert!(server.secret(CLOUD_ENDPOINT_RUNTIME_CONFIG).is_none() && server.secret(CLOUD_ENDPOINT_CONFIRMED_ORIGIN).is_none());
    assert_eq!(server.secret(CLOUD_CLI_DESIRED_LINK_SECRET).as_deref(), Some("managed"), "the wish to be linked survives");
}

/// Runs a server over a managed link to its shutdown with `prepare` having set up the markers under `<home>/runtime`, and says
/// whether the tunnel was released.
async fn released_after(prepare: impl FnOnce(&std::path::Path)) -> (bool, std::path::PathBuf) {
    let plane = Recorder::new();
    // The markers carry real file times, so the connect clock is the real one for these.
    let server = start(Options { start: false, plane: Some(plane.clone()), clock_ms: Utc::now().timestamp_millis(), ..Options::new(no_relay) }).await;
    for (name, value) in connect_support::linked(&server.relay.url, &server.relay_keys) {
        server.connect.secrets.set(&name, value.as_bytes()).unwrap();
    }
    managed(&server);
    let runtime = server.base.join("runtime");
    std::fs::create_dir_all(&runtime).unwrap();
    prepare(&runtime);
    server.finish().await;
    (plane.count("release") == 1, runtime)
}

#[tokio::test]
async fn shutdown_keeps_the_tunnel_only_across_a_restart_for_an_update() {
    use t3_contracts::DESKTOP_UPDATE_RESTART_MARKER_FILE as MARKER;
    // Nothing says an update is coming: released.
    assert!(released_after(|_| {}).await.0);
    // The desktop app wrote its marker just now: kept, and the marker is consumed by being read.
    let (released, runtime) = released_after(|runtime| std::fs::write(runtime.join(MARKER), b"").unwrap()).await;
    assert!(!released);
    assert!(!runtime.join(MARKER).exists());
    // A marker from a quit that never read it (two minutes old) does not keep the tunnel on a later quit.
    let (released, runtime) = released_after(|runtime| {
        let marker = runtime.join(MARKER);
        std::fs::write(&marker, b"").unwrap();
        std::fs::File::options().write(true).open(&marker).unwrap().set_modified(std::time::SystemTime::now() - Duration::from_secs(120)).unwrap();
    })
    .await;
    assert!(released);
    assert!(!runtime.join(MARKER).exists(), "an old marker is removed all the same");
    // A launcher with an update pending keeps it, unless the launcher marked an explicit stop.
    let pending = |runtime: &std::path::Path| std::fs::write(runtime.join("service-state.json"), r#"{"update":{"status":"pending"}}"#).unwrap();
    assert!(!released_after(pending).await.0);
    assert!(released_after(|runtime| {
        pending(runtime);
        std::fs::write(runtime.join(".service-stopping"), b"").unwrap();
    })
    .await
    .0);
    assert!(released_after(|runtime| std::fs::write(runtime.join("service-state.json"), r#"{"update":{"status":"done"}}"#).unwrap()).await.0);
}

#[tokio::test(start_paused = true)]
async fn expired_replay_markers_are_swept_when_the_server_starts_and_every_hour() {
    let server = start(Options { start: false, ..Options::new(no_relay) }).await;
    let secrets = server.state_dir.join("secrets");
    let old = |name: &str| {
        let path = secrets.join(name);
        std::fs::write(&path, b"x").unwrap();
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(std::time::SystemTime::now() - Duration::from_secs(2 * 24 * 3600)).unwrap();
        path
    };
    let (first, kept) = (old("dpop-proof-first.bin"), old("cloud-relay-url.bin"));
    server.connect.start();
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert!(!first.exists(), "swept at activation");
    assert!(kept.exists(), "only markers go");
    let second = old("cloud-mint-jti-second.bin");
    let fresh = secrets.join("cloud-health-nonce-fresh.bin");
    std::fs::write(&fresh, b"x").unwrap();
    tokio::time::sleep(Duration::from_secs(3600)).await;
    assert!(!second.exists() && fresh.exists());
    server.finish().await;
}

// ---- the webhook route ---------------------------------------------------------------------------------

fn delivery_proof(server: &Server, hook_id: &str, delivery: &str, received_at: &str) -> String {
    let now = NOW_MS / 1000;
    let claims = json!({"iss":server.relay.url,"aud":"t3-env:env_123","sub":"env_123","jti":"j","iat":now - 3600,"exp":now + 3600,"environmentId":"env_123","deliveryId":delivery,"receivedAt":received_at,"hookId":hook_id});
    sign_relay_jwt(&server.relay_keys.private_key, RELAY_HOOK_DELIVERY_TYP, &claims).unwrap()
}

#[tokio::test]
async fn a_webhook_counts_as_relay_delivered_only_with_the_relays_signed_proof() {
    use t3_server::scheduled_tasks::{ManualClock, RunLauncher, ScheduledTasks};
    struct Launcher;
    impl RunLauncher for Launcher {
        fn launch(&self, _: t3_server::scheduled_tasks::LaunchRequest) -> futures_util::future::BoxFuture<'static, Result<String, String>> {
            Box::pin(async { Ok("thread".to_owned()) })
        }
    }
    let server = started(Recorder::new(), &[]).await;
    let service = ScheduledTasks::of(&server.store).unwrap();
    service.set_launcher(Arc::new(Launcher));
    let _ = ManualClock::new(0);
    let mut socket = {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let mut request = format!("ws://{}/ws?orchestrationProtocol=2", server.address).into_client_request().unwrap();
        request.headers_mut().insert("Authorization", format!("Bearer {}", server.operator).parse().unwrap());
        tokio_tungstenite::connect_async(request).await.unwrap().0
    };
    let task = json!({"id":"scheduled-task:hook","title":"Hook","prompt":"Go","enabled":true,"schedule":{"type":"webhook"},"projectId":"project","workspaceStrategy":{"type":"root"},"modelSelection":{"instanceId":"codex","model":"m"},"runtimeMode":"approval-required","interactionMode":"default"});
    let created = rpc(&mut socket, 1, "scheduledTasks.upsert", task.clone()).await;
    let path = success(&created)["task"]["webhook"]["path"].as_str().unwrap_or_else(|| panic!("{created}")).to_owned();
    let hook = "scheduled-task:hook";
    let url = server.url(&path);
    let post = |headers: Vec<(&'static str, String)>| {
        let mut request = client().post(&url).body("{}");
        for (name, value) in headers {
            request = request.header(name, value);
        }
        async move { request.send().await.unwrap() }
    };
    let delivery_of = |response: reqwest::Response| async move { response.json::<Value>().await.unwrap()["deliveryId"].as_str().unwrap().to_owned() };
    let relay_headers = |proof: String, delivery: &str| vec![(RELAY_HOOK_DELIVERY_HEADER, proof), ("x-t3-relay-delivery-id", delivery.to_owned()), ("x-t3-relay-received-at", "2026-01-01T00:00:00.000Z".to_owned())];

    // Someone calling the URL directly is a direct request, whatever headers they send.
    let direct = delivery_of(post(vec![("x-t3-relay-delivery-id", "d-1".into()), ("x-t3-relay-received-at", "2026-01-01T00:00:00.000Z".into())]).await).await;
    assert!(direct.starts_with("delivery:") && !direct.starts_with("delivery:relay:"), "{direct}");
    // The relay's proof makes the delivery id its own, and the same delivery is not run twice.
    let proof = delivery_proof(&server, hook, "d-2", "2026-01-01T00:00:00.000Z");
    let relayed = post(relay_headers(proof.clone(), "d-2")).await;
    assert_eq!((relayed.status().as_u16(), relayed.headers()["x-t3-hook-outcome"].to_str().unwrap().to_owned()), (202, "accepted".to_owned()));
    assert_eq!(delivery_of(relayed).await, "delivery:relay:d-2");
    let again = post(relay_headers(proof.clone(), "d-2")).await;
    assert_eq!(again.headers()["x-t3-hook-outcome"], "duplicate");
    // A proof for another delivery, another hook, or signed by anyone else is not believed.
    let other = delivery_of(post(relay_headers(proof, "d-3")).await).await;
    assert!(!other.starts_with("delivery:relay:"), "{other}");
    let elsewhere = delivery_of(post(relay_headers(delivery_proof(&server, "scheduled-task:other", "d-4", "2026-01-01T00:00:00.000Z"), "d-4")).await).await;
    assert!(!elsewhere.starts_with("delivery:relay:"), "{elsewhere}");
    let forged = sign_relay_jwt(&KeyPair::generate().private_key, RELAY_HOOK_DELIVERY_TYP, &json!({"iss":server.relay.url,"aud":"t3-env:env_123","iat":NOW_MS / 1000})).unwrap();
    assert!(!delivery_of(post(relay_headers(forged, "d-5")).await).await.starts_with("delivery:relay:"));
    // A task shows the relay URL of its tunnel once there is one.
    let hook_url = |task: &Value| task["task"]["webhook"]["url"].clone();
    assert_eq!(hook_url(&success(&rpc(&mut socket, 2, "scheduledTasks.upsert", task.clone()).await)), Value::Null);
    for (name, value) in [
        (CLOUD_ENDPOINT_RUNTIME_CONFIG, r#"{"providerKind":"cloudflare_tunnel","connectorToken":"c","tunnelId":"t","tunnelName":"t3-env-0123456789abcdef"}"#),
    ] {
        server.connect.secrets.set(name, value.as_bytes()).unwrap();
    }
    let linked_url = hook_url(&success(&rpc(&mut socket, 3, "scheduledTasks.upsert", task).await));
    assert!(linked_url.as_str().unwrap().starts_with(&format!("{}/v1/hooks/0123456789abcdef/scheduled-task%3Ahook/", server.relay.url)), "{linked_url}");
    server.finish().await;
}

// ---- Tailscale Serve -------------------------------------------------------------------------------------

#[cfg(unix)]
#[tokio::test]
async fn tailscale_serve_is_configured_and_switched_off_only_if_this_server_switched_it_on() {
    use t3_remote::tailscale::Tailscale;
    use t3_server::connect::TailscaleServe;
    let directory = tempfile::Builder::new().prefix("t3code-rust-port-test-tailscale-").tempdir().unwrap();
    let log = directory.path().join("calls.log");
    let script = |name: &str, body: &str| {
        let path = remote::write_script(directory.path(), name, &format!("echo \"$@\" >> '{}'\n{body}", log.display()));
        path.to_string_lossy().into_owned()
    };
    // It works: `serve --bg --https=8443 http://127.0.0.1:4000`, and off again at the end.
    let working = Arc::new(Tailscale::with_executable(script("tailscale-ok", "exit 0")));
    let serve = TailscaleServe::activate(working, 4000, 8443);
    serve.deactivate().await;
    assert_eq!(std::fs::read_to_string(&log).unwrap().lines().collect::<Vec<_>>(), ["serve --bg --https=8443 http://127.0.0.1:4000", "serve --https=8443 off"]);
    // It does not: the server runs without it, and leaves alone what it did not set up.
    std::fs::remove_file(&log).unwrap();
    let failing = Arc::new(Tailscale::with_executable(script("tailscale-fail", "echo 'not logged in' >&2\nexit 1")));
    TailscaleServe::activate(failing, 4000, 443).deactivate().await;
    assert_eq!(std::fs::read_to_string(&log).unwrap().lines().collect::<Vec<_>>(), ["serve --bg --https=443 http://127.0.0.1:4000"]);
}

// ---- agent activity -------------------------------------------------------------------------------------

mod activity {
    use super::*;
    use t3_server::{project::ProjectService, thread::ThreadService};

    pub struct Fixture {
        pub server: Server,
        pub plane: Arc<Recorder>,
        pub threads: ThreadService,
    }

    pub async fn fixture(publishing: bool) -> Fixture {
        let plane = Recorder::new();
        let extra: Vec<(&str, String)> = if publishing { vec![(PUBLISH_AGENT_ACTIVITY_SECRET, "true".into())] } else { vec![] };
        let server = started_with_clock(plane.clone(), &extra, Some(Arc::new(t3_remote::clock::VirtualClock::new(Utc::now().timestamp_millis())))).await;
        let workspace = server.base.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        ProjectService::new(server.store.clone())
            .mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Project","workspaceRoot":workspace}), Utc::now())
            .unwrap();
        let threads = ThreadService::new(server.store.clone());
        Fixture { server, plane, threads }
    }

    impl Fixture {
        pub fn create_thread(&self, id: &str) {
            let receipt = self
                .threads
                .dispatch(&json!({"type":"thread.create","commandId":format!("create-{id}"),"threadId":id,"projectId":"project","title":"Fix the bug","modelSelection":{"instanceId":"codex","model":"gpt-5"},"runtimeMode":"approval-required","interactionMode":"default","branch":null,"worktreePath":null}), Utc::now())
                .unwrap();
            assert_eq!(receipt.status, "accepted");
        }

        /// A turn that has been asked for and is being prepared: the shell shows the thread as starting.
        pub fn start_turn(&self, id: &str) {
            let command = json!({"type":"message.dispatch","commandId":format!("turn-{id}"),"threadId":id,"messageId":format!("message-{id}"),"text":"Prepared turn","attachments":[],"createdBy":"user","creationSource":"web","dispatchMode":{"type":"defer_start","workspaceStrategy":{"type":"root"}}});
            let now = Utc::now();
            self.server
                .store
                .dispatch(
                    &format!("turn-{id}"),
                    "thread",
                    id,
                    "message.dispatch",
                    now,
                    |tx| t3_server::execution::plan_message(&command, &t3_server::persistence::read_projection(tx, "thread", id)?.unwrap(), now),
                    t3_server::thread::reduce,
                )
                .unwrap();
        }

        pub fn dispatch(&self, command: Value) {
            assert_eq!(self.threads.dispatch(&command, Utc::now()).unwrap().status, "accepted");
        }

        pub fn publishes(&self) -> Vec<(String, Value, String)> {
            self.plane.publishes.lock().unwrap().clone()
        }
    }
}

#[tokio::test(start_paused = true)]
async fn active_threads_are_published_at_start_and_later_changes_follow_the_events() {
    let f = activity::fixture(true).await;
    f.create_thread("thread-1");
    f.start_turn("thread-1");
    f.server.connect.start();
    // The catch-up, a second after start: the thread being prepared is active.
    f.plane.wait_for("publish", 1).await;
    let (thread, state, proof) = f.publishes()[0].clone();
    assert_eq!(thread, "thread-1");
    assert_eq!((state["phase"].as_str(), state["headline"].as_str(), state["projectTitle"].as_str(), state["threadTitle"].as_str()), (Some("starting"), Some("Starting agent"), Some("Project"), Some("Fix the bug")));
    assert_eq!((state["modelTitle"].as_str(), state["deepLink"].as_str(), state["environmentId"].as_str()), (Some("gpt-5"), Some("/threads/env_123/thread-1"), Some("env_123")));
    assert!(f.plane.count("publish:environment-credential") == 1, "published with the environment credential");
    // The proof is the environment's, for this thread and this state.
    let keys = t3_remote::env_keys::get_or_create_environment_key_pair(f.server.connect.secrets.as_ref()).unwrap();
    let claims = verify_relay_jwt(&VerifyOptions { public_key: &keys.public_key, token: &proof, typ: RELAY_ACTIVITY_PUBLISH_TYP, issuer: "t3-env:env_123", audience: &f.server.relay.url, now_epoch_seconds: Utc::now().timestamp(), max_token_age_secs: None }).unwrap();
    assert_eq!((claims["threadId"].as_str(), &claims["state"]), (Some("thread-1"), &state));

    // A retitle is an event that can change the state: the new title is published.
    f.dispatch(json!({"type":"thread.metadata.update","commandId":"retitle","threadId":"thread-1","title":"Fix the other bug"}));
    f.plane.wait_for("publish", 2).await;
    assert_eq!(f.publishes()[1].1["threadTitle"], "Fix the other bug");
    // A pin changes nothing the relay shows: no publish. (Another retitle after it proves the pin was seen and passed over.)
    f.dispatch(json!({"type":"thread.pin","commandId":"pin","threadId":"thread-1"}));
    f.dispatch(json!({"type":"thread.metadata.update","commandId":"retitle-2","threadId":"thread-1","title":"Fix it"}));
    f.plane.wait_for("publish", 3).await;
    assert_eq!(f.publishes().iter().map(|(_, state, _)| state["threadTitle"].as_str().unwrap().to_owned()).collect::<Vec<_>>(), ["Fix the bug", "Fix the other bug", "Fix it"]);

    // Archiving ends the thread's activity. A tombstone is confirmed before it is sent: the projection may be mid-write.
    f.dispatch(json!({"type":"thread.archive","commandId":"archive","threadId":"thread-1"}));
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(f.plane.count("publish"), 3, "not yet");
    f.plane.wait_for("publish", 4).await;
    assert_eq!(f.publishes()[3].1, Value::Null);
    f.server.finish().await;
}

#[tokio::test(start_paused = true)]
async fn nothing_is_published_until_the_environment_is_linked_and_publishing_is_on() {
    let f = activity::fixture(false).await;
    f.create_thread("thread-1");
    f.start_turn("thread-1");
    f.server.connect.start();
    tokio::time::sleep(Duration::from_secs(30)).await;
    f.dispatch(json!({"type":"thread.metadata.update","commandId":"retitle","threadId":"thread-1","title":"Other"}));
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(f.plane.count("publish"), 0, "publishing is off");
    assert!(f.server.connect.services.agent_activity_publishing_active() == false);
    // Turning it on, as `t3 connect publish` or the client's toggle does, wakes the catch-up at once.
    f.server.connect.secrets.set(PUBLISH_AGENT_ACTIVITY_SECRET, b"true").unwrap();
    let (status, _) = {
        let response = client().post(f.server.url("/api/connect/preferences")).header("authorization", format!("Bearer {}", f.server.writer)).json(&json!({"publishAgentActivity":true})).send().await.unwrap();
        (response.status().as_u16(), ())
    };
    assert_eq!(status, 200);
    f.plane.wait_for("publish", 1).await;
    assert_eq!(f.publishes()[0].1["threadTitle"], "Other");
    f.server.finish().await;
}
