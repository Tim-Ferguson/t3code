//! The usage providers on the Providers page through the mounted UI and the real native server: a
//! CLIProxyAPI hub added with its key, the key kept off the settings the window reads back, the
//! accounts it pools in the Limits view and the `/usage-limits` command the server advertises for
//! them, removal behind its confirmation, and the health check interval. The hub is a scripted
//! loopback origin; nothing leaves the machine.
use super::diff_tests::{Fixture, Harness, rendered_text, until, wait_for_rendered_text};
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::keys_tests::key_on;
use crate::runtime::transport_tests::{change_control, click_control, control, control_attribute, input_control};
use std::{
    io::{Read, Write},
    sync::{Arc, Mutex},
};

const SCOPES: [AuthEnvironmentScope; 5] = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::DiagnosticsRead, AuthEnvironmentScope::ProvidersManage, AuthEnvironmentScope::SettingsWrite];
const READER: [AuthEnvironmentScope; 1] = [AuthEnvironmentScope::OrchestrationRead];
const KEY: &str = "management-secret-for-the-ui-test";

/// A loopback hub with one Codex account (a Pro plan, a session and a weekly window, one reset credit). It records the
/// management key each request carried.
struct Hub {
    url: String,
    keys: Arc<Mutex<Vec<String>>>,
}
impl Hub {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let keys = Arc::new(Mutex::new(Vec::new()));
        let seen = keys.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                // The head and the body may arrive in separate reads.
                let mut received = Vec::new();
                loop {
                    let mut buffer = [0u8; 8192];
                    let read = stream.read(&mut buffer).unwrap_or(0);
                    received.extend_from_slice(&buffer[..read]);
                    let text = String::from_utf8_lossy(&received).to_string();
                    let complete = text.split_once("\r\n\r\n").is_some_and(|(head, body)| {
                        let length = head.lines().find_map(|line| line.to_lowercase().strip_prefix("content-length: ").and_then(|value| value.trim().parse::<usize>().ok())).unwrap_or(0);
                        body.len() >= length
                    });
                    if read == 0 || complete {
                        break;
                    }
                }
                let request = String::from_utf8_lossy(&received).to_string();
                let (head, body) = request.split_once("\r\n\r\n").unwrap_or((&request, ""));
                let target = head.lines().next().unwrap_or("").split(' ').nth(1).unwrap_or("").to_owned();
                if let Some(key) = head.lines().find_map(|line| line.to_lowercase().strip_prefix("authorization: bearer ").map(str::to_owned)) {
                    seen.lock().unwrap().push(key);
                }
                let wanted: serde_json::Value = serde_json::from_str(body).unwrap_or(json!({}));
                let upstream = |status: u16, body: serde_json::Value| json!({"status_code":status,"body":body.to_string()});
                let answer = match target.as_str() {
                    "/v0/management/auth-files" => json!({"files":[{"id":"a.json","auth_index":"a","provider":"codex","email":"a@example.com","id_token":{"chatgpt_account_id":"acct-a"}}]}),
                    "/v0/management/api-call" => match wanted["url"].as_str() {
                        Some("https://chatgpt.com/backend-api/wham/usage") => upstream(200, json!({"plan_type":"pro","rate_limit":{"primary_window":{"used_percent":12.5,"reset_at":4_070_908_800u64,"limit_window_seconds":18000},"secondary_window":{"used_percent":78,"reset_at":4_070_908_800u64,"limit_window_seconds":604_800}}})),
                        Some("https://chatgpt.com/backend-api/wham/rate-limit-reset-credits") => upstream(200, json!({"credits":[{"id":"first","expires_at":"2099-01-01T00:00:00Z","status":"available","reset_type":"codex_rate_limits"}]})),
                        _ => json!({}),
                    },
                    _ => json!({}),
                };
                let body = answer.to_string();
                let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            }
        });
        Self { url, keys }
    }
}

async fn open(fixture: &Fixture, scopes: &[AuthEnvironmentScope]) -> (VirtualDom, Harness, Store<UiModel>) {
    let (mut dom, props) = fixture.client(Gate::default(), scopes);
    let state = props.state.borrow().unwrap();
    until(&mut dom, "the shell", |_| state.peek().shell.synchronized).await;
    until(&mut dom, "the configuration", |_| state.peek().typed_config.is_some()).await;
    state.view().set(View::Providers);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    wait_for_rendered_text(&mut dom, "Usage providers").await;
    (dom, props, state)
}
fn disabled(dom: &VirtualDom, label: &str) -> bool {
    control_attribute(dom, label, "disabled").and_then(|(_, value)| value).as_deref() == Some("true")
}
fn hubs(state: Store<UiModel>) -> serde_json::Map<String, Value> {
    state.config().peek()["settings"]["usageLimitSources"].as_object().cloned().unwrap_or_default()
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_hub_is_added_with_its_key_pools_its_accounts_and_is_removed_behind_a_confirmation() {
    let hub = Hub::start();
    let fixture = Fixture::start_with_usage_sources().await;
    let (mut dom, _props, state) = open(&fixture, &SCOPES).await;
    assert!(rendered_text(&dom).contains("No hubs configured."));

    // The dialog asks for the address and the key before it lets the hub be added; Escape and Cancel leave nothing.
    click_control(&mut dom, "Add hub");
    assert!(disabled(&dom, "Add hub to the list"));
    input_control(&mut dom, "Hub URL", &hub.url);
    assert!(disabled(&dom, "Add hub to the list"), "an address alone is not enough");
    key_on(&mut dom, "Add a CLIProxyAPI hub", Key::Escape);
    assert!(control(&dom, "Hub URL").is_none() && hubs(state).is_empty());
    click_control(&mut dom, "Add hub");
    assert_eq!(control(&dom, "Hub URL").and_then(|(_, value)| value).unwrap_or_default(), "", "a new dialog starts empty");
    click_control(&mut dom, "Cancel adding a hub");
    assert!(control(&dom, "Hub URL").is_none() && hubs(state).is_empty());

    click_control(&mut dom, "Add hub");
    input_control(&mut dom, "Hub URL", &hub.url);
    input_control(&mut dom, "Management key", KEY);
    input_control(&mut dom, "Label", "Proxy");
    assert!(!disabled(&dom, "Add hub to the list"));
    click_control(&mut dom, "Add hub to the list");
    assert!(control(&dom, "Hub URL").is_none(), "the dialog closes with the click");
    let address = format!("CLI Proxy \u{b7} {}", hub.url);
    wait_for_rendered_text(&mut dom, &address).await;
    let text = rendered_text(&dom);
    assert!(text.contains("Proxy") && !text.contains("No hubs configured."), "{text}");

    // The server keeps the key: settings read back carry no copy of it, and the hub was asked with it.
    let entries = hubs(state);
    assert_eq!(entries.len(), 1);
    let (id, entry) = entries.iter().next().unwrap();
    assert!(id.starts_with("cliproxy-127.0.0.1-"), "{id}");
    assert_eq!((entry["kind"].as_str(), entry["label"].as_str(), entry["url"].as_str()), (Some("cliproxy"), Some("Proxy"), Some(hub.url.as_str())));
    assert!(!entry.to_string().contains(KEY), "the key is not in the settings: {entry}");
    until(&mut dom, "the hub to be read", |_| !hub.keys.lock().unwrap().is_empty()).await;
    assert!(hub.keys.lock().unwrap().iter().all(|key| key == KEY));

    // Its account reaches the Limits view, and the server offers `/usage-limits` on the provider it reports on.
    until(&mut dom, "the command in the provider catalog", |_| {
        state.typed_config().peek().as_ref().is_some_and(|config| config.providers.0.iter().any(|provider| provider.driver.as_str() == "codex" && provider.slash_commands.iter().any(|command| command.name.as_str() == "usage-limits")))
    })
    .await;
    state.view().set(View::Usage);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    wait_for_rendered_text(&mut dom, "Session88%").await;
    assert!(rendered_text(&dom).contains("\u{1f39f}1"), "the hub's banked credit: {}", rendered_text(&dom));
    state.view().set(View::Providers);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);

    // Remove asks first; declining keeps the hub, confirming deletes it and its account leaves the Limits view.
    click_control(&mut dom, "Remove Proxy");
    assert!(rendered_text(&dom).contains("Remove Proxy?") && rendered_text(&dom).contains("The hub's management key is deleted from this server."));
    click_control(&mut dom, "Cancel hub removal");
    assert!(!rendered_text(&dom).contains("Remove Proxy?") && hubs(state).len() == 1);
    click_control(&mut dom, "Remove Proxy");
    key_on(&mut dom, "Remove Proxy?", Key::Escape);
    assert!(!rendered_text(&dom).contains("Remove Proxy?") && hubs(state).len() == 1, "Escape declines");
    click_control(&mut dom, "Remove Proxy");
    click_control(&mut dom, "Remove hub");
    wait_for_rendered_text(&mut dom, "No hubs configured.").await;
    assert!(hubs(state).is_empty());
    until(&mut dom, "the command to leave the catalog", |_| {
        state.typed_config().peek().as_ref().is_some_and(|config| config.providers.0.iter().all(|provider| provider.slash_commands.iter().all(|command| command.name.as_str() != "usage-limits")))
    })
    .await;
    state.view().set(View::Usage);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    until(&mut dom, "the account to leave the Limits view", |dom| !rendered_text(dom).contains("Session88%")).await;
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_connection_that_cannot_manage_providers_sees_the_hubs_but_cannot_change_them() {
    let fixture = Fixture::start(false).await;
    let (mut dom, _props, _state) = open(&fixture, &READER).await;
    assert!(rendered_text(&dom).contains("No hubs configured."));
    assert!(control(&dom, "Add hub").is_none());
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn the_health_check_interval_steps_by_thirty_seconds_and_goes_back_to_the_presets() {
    let fixture = Fixture::start(false).await;
    let (mut dom, _props, state) = open(&fixture, &SCOPES).await;
    let seconds = |dom: &VirtualDom| control(dom, "Provider health check interval in seconds").and_then(|(_, value)| value).unwrap_or_default();
    until(&mut dom, "the interval", |dom| !seconds(dom).is_empty()).await;
    assert_eq!(seconds(&dom), "300", "the balanced preset is five minutes");
    assert!(control(&dom, "Reset provider health check interval").is_none());
    click_control(&mut dom, "Increase provider health check interval");
    until(&mut dom, "330 seconds", |dom| seconds(dom) == "330").await;
    assert_eq!(state.config().peek()["settings"]["backgroundActivity"]["overrides"]["providerHealthRefreshInterval"], 330_000);
    change_control(&mut dom, "Provider health check interval in seconds", "0");
    until(&mut dom, "no checks", |dom| seconds(dom) == "0").await;
    assert!(disabled(&dom, "Decrease provider health check interval"));
    click_control(&mut dom, "Reset provider health check interval");
    until(&mut dom, "the preset", |dom| seconds(dom) == "300").await;
    assert!(control(&dom, "Reset provider health check interval").is_none());
    fixture.finish(dom).await;
}
