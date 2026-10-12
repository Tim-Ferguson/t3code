//! The Usage page through the mounted UI and the real native server: the transcript scan read
//! over Codex history under an isolated home and priced from a loopback rate table, the
//! cost/tokens/period controls and their shortcuts, the chart and breakdown, limits pooled from
//! the Claude fixture's windows, and a banked reset credit redeemed once per confirmation.
//! Waits are on text that only the awaited step produces.
use super::diff_tests::{Fixture, Harness, rendered_text, until, wait_for_rendered_text};
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::keys_tests::{key_on, open_palette, press, typed};
use t3_client::keybindings::{Dom, KeyEvent};
use crate::runtime::transport_tests::{click_control, control, control_attribute};
use std::{
    io::{Read, Write},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
};

const SCOPES: [AuthEnvironmentScope; 4] = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::DiagnosticsRead, AuthEnvironmentScope::ProvidersManage];
const READER: [AuthEnvironmentScope; 1] = [AuthEnvironmentScope::OrchestrationRead];

/// A loopback HTTP origin answering each request from `answer(method, target)`.
struct Origin {
    url: String,
}
impl Origin {
    fn start(answer: impl Fn(&str, &str) -> (u16, String) + Send + 'static) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut buffer = [0u8; 8192];
                let read = stream.read(&mut buffer).unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                let mut head = request.lines().next().unwrap_or("").split(' ');
                let (method, target) = (head.next().unwrap_or(""), head.next().unwrap_or(""));
                let (status, body) = answer(method, target);
                let _ = write!(stream, "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            }
        });
        Self { url }
    }
}

/// The one rate table every usage test prices against, served from loopback.
fn rates() -> &'static str {
    static ORIGIN: OnceLock<Origin> = OnceLock::new();
    &ORIGIN
        .get_or_init(|| {
            let origin = Origin::start(|_, _| (200, json!({"gpt-5":{"input_cost_per_token":1.25e-6,"output_cost_per_token":10e-6,"cache_read_input_token_cost":1.25e-7}}).to_string()));
            // The server reads this on each call; the process-wide value is set before any test reads it.
            unsafe { std::env::set_var("T3CODE_USAGE_RATES_URL", &origin.url) };
            origin
        })
        .url
}

fn stamp(minutes: i64) -> String {
    (chrono::Utc::now() - chrono::Duration::minutes(minutes)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
/// One Codex session of one model: 1,000,000 input tokens (400,000 of them cached) and 50,000 output.
fn write_codex_history(fixture: &Fixture) {
    write_codex_history_of(fixture, "gpt-5");
}
/// The same session on another model, which the rate table may not price.
fn write_codex_history_of(fixture: &Fixture, model: &str) {
    let root = fixture.workspace.parent().unwrap();
    let lines = [
        json!({"type":"session_meta","timestamp":stamp(15),"payload":{"id":"codex-session"}}),
        json!({"type":"turn_context","timestamp":stamp(14),"payload":{"model":model}}),
        json!({"type":"event_msg","timestamp":stamp(13),"payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":1_000_000,"cached_input_tokens":400_000,"output_tokens":50_000,"reasoning_output_tokens":30_000}}}}),
    ];
    let path = root.join("codex-home/sessions/2026/10/09/rollout-1.jsonl");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, lines.iter().map(|line| line.to_string() + "\n").collect::<String>()).unwrap();
}

async fn open(fixture: &Fixture, scopes: &[AuthEnvironmentScope]) -> (VirtualDom, Harness, Store<UiModel>) {
    let (mut dom, props) = fixture.client(Gate::default(), scopes);
    let state = props.state.borrow().unwrap();
    until(&mut dom, "the shell", |_| state.peek().shell.synchronized).await;
    until(&mut dom, "the configuration", |_| state.peek().typed_config.is_some()).await;
    state.view().set(View::Usage);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    until(&mut dom, "the saved choices", |_| state.usage().peek().hydrated).await;
    (dom, props, state)
}
fn pressed(dom: &VirtualDom, label: &str) -> bool {
    control_attribute(dom, label, "aria-pressed").and_then(|(_, value)| value).as_deref() == Some("true")
}
fn disabled(dom: &VirtualDom, label: &str) -> bool {
    control_attribute(dom, label, "disabled").and_then(|(_, value)| value).as_deref() == Some("true")
}
async fn loaded_days(dom: &mut VirtualDom, state: Store<UiModel>, days: i64) {
    until(dom, "a scan of the period", |_| state.usage().peek().loaded.as_ref().is_some_and(|loaded| loaded.0.days == days) && !state.usage().peek().scanning).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn the_scan_prices_codex_history_and_the_page_draws_cost_tokens_the_chart_and_the_breakdown() {
    let _ = rates();
    let fixture = Fixture::start(false).await;
    write_codex_history(&fixture);
    let (mut dom, _props, state) = open(&fixture, &SCOPES).await;
    // Limits is the first-visit page; Cost is one click away and starts the read honestly.
    assert!(pressed(&dom, "Limits") && disabled(&dom, "7 days"), "the period does not apply to limits");
    click_control(&mut dom, "Cost");
    assert!(rendered_text(&dom).contains("Scanning transcripts"), "the read says it is scanning, with no invented progress");
    assert!(disabled(&dom, "Refresh usage"), "a read in flight cannot be started again");
    loaded_days(&mut dom, state, 30).await;
    wait_for_rendered_text(&mut dom, "gpt-5").await;
    let text = rendered_text(&dom);
    assert!(!text.contains("Scanning transcripts"), "the scanning note ends with the scan");
    // 600,000 uncached at 1.25e-6, 400,000 cached at 1.25e-7 and 50,000 output at 1e-5.
    assert!(text.contains("$1.30"), "{text}");
    assert!(text.contains("1 sessions") && text.contains("API estimate") && text.contains("1 session"), "{text}");
    assert!(text.contains("Codex") && text.contains("100.0% of cost \u{b7} 1.05M tokens"), "{text}");
    assert!(text.contains("Processed tokens") && text.contains("Cache savings") && text.contains("Cost by type"), "{text}");
    assert!(control(&dom, "Daily cost by provider").is_some(), "the chart is an SVG with a label");

    // Tokens re-reads nothing: the window is the same.
    let generation = state.usage().peek().generation;
    click_control(&mut dom, "Tokens");
    assert!(pressed(&dom, "Tokens") && rendered_text(&dom).contains("1.05M") && control(&dom, "Daily processed tokens by provider").is_some());
    assert!(rendered_text(&dom).contains("Tokens by type"));
    assert_eq!(state.usage().peek().generation, generation, "a metric change reads nothing");

    // A new period reads again and the page keeps the result of the latest read.
    click_control(&mut dom, "7 days");
    click_control(&mut dom, "90 days");
    loaded_days(&mut dom, state, 90).await;
    assert!(state.usage().peek().generation >= generation + 2);
    assert!(pressed(&dom, "90 days") && !pressed(&dom, "7 days"));

    // The breakdown by day lists the day; by model opens a model with its own mixes.
    click_control(&mut dom, "By day");
    let today = chrono::Local::now().format("%b %-d").to_string();
    assert!(rendered_text(&dom).contains(&today), "{today} in {}", rendered_text(&dom));
    click_control(&mut dom, "By model");
    click_control(&mut dom, "Open gpt-5");
    assert!(control(&dom, "gpt-5").is_some() && rendered_text(&dom).contains("Cache hit") && rendered_text(&dom).contains("40.0%"), "{}", rendered_text(&dom));
    key_on(&mut dom, "gpt-5", Key::Escape);
    assert!(control(&dom, "gpt-5").is_none(), "Escape closes the model");
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn shortcuts_and_the_palette_open_the_page_and_move_between_its_views() {
    let _ = rates();
    let fixture = Fixture::start(false).await;
    write_codex_history(&fixture);
    let (mut dom, _props, state) = open(&fixture, &SCOPES).await;
    state.view().set(View::Chat);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    // The palette offers the page with its shortcut.
    open_palette(&mut dom).await;
    wait_for_rendered_text(&mut dom, "Open usage").await;
    click_control(&mut dom, "Open usage");
    until(&mut dom, "the page", |_| *state.view().peek() == View::Usage).await;
    // Its letters work on the page and are left alone in a field or a dialog.
    assert_eq!(press(&mut dom, typed("c"), Dom::default()).await.as_deref(), Some("usage.cost"));
    loaded_days(&mut dom, state, 30).await;
    assert_eq!(press(&mut dom, typed("t"), Dom { editable_focus: true, ..Default::default() }).await, None, "a plain letter in a field is typed, not a command");
    assert_eq!(press(&mut dom, typed("t"), Dom { dialog_open: true, ..Default::default() }).await, None, "nor does one reach past a dialog");
    assert_eq!(state.usage().peek().metric, "cost");
    assert_eq!(press(&mut dom, typed("t"), Dom::default()).await.as_deref(), Some("usage.tokens"));
    assert_eq!(state.usage().peek().metric, "tokens");
    let generation = state.usage().peek().generation;
    assert_eq!(press(&mut dom, KeyEvent { key: "2".into(), code: "Digit2".into(), shift: true, ..crate::runtime::keys_tests::chord("2", true, false) }, Dom::default()).await.as_deref(), Some("usage.period.week"));
    loaded_days(&mut dom, state, 7).await;
    assert!(state.usage().peek().generation > generation, "a new period reads again");
    assert_eq!(press(&mut dom, typed("l"), Dom::default()).await.as_deref(), Some("usage.limits"));
    assert!(pressed(&dom, "Limits") && disabled(&dom, "7 days"));
    fixture.finish(dom).await;
}

/// A reader of the environment cannot ask for usage, and says why.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_connection_without_diagnostics_access_cannot_read_usage() {
    let fixture = Fixture::start(false).await;
    let (mut dom, _props, state) = open(&fixture, &READER).await;
    click_control(&mut dom, "Cost");
    wait_for_rendered_text(&mut dom, "This connection cannot read usage.").await;
    assert!(!state.usage().peek().scanning && state.usage().peek().loaded.is_none());
    assert!(disabled(&dom, "Refresh usage"));
    fixture.finish(dom).await;
}

fn usage_reply() -> String {
    json!({"rate_limits_available":true,"rate_limits":{"five_hour":{"utilization":10,"resets_at":"2030-01-01T05:00:00Z"},"seven_day":{"utilization":20,"resets_at":null},"model_scoped":[{"display_name":"Fable 5","utilization":30,"resets_at":"2030-01-05T00:00:00.000Z"}]}}).to_string()
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn limits_are_pooled_per_provider_and_a_reset_credit_is_spent_once_per_confirmation() {
    let future = (chrono::Utc::now() + chrono::Duration::days(30)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let claims = Arc::new(AtomicUsize::new(0));
    let origin = {
        let (future, claims) = (future.clone(), claims.clone());
        Origin::start(move |method, target| {
            if target.starts_with("/api/oauth/usage") {
                (200, json!({"cedar_ember":{"eligible":true,"grants":[{"id":"g_1","resets_left":2,"usable_now":true,"ends_at":future}],"next_grant_id":"g_1"}}).to_string())
            } else if method == "POST" && target == "/api/organizations/org-1/reset_rate_limits" {
                claims.fetch_add(1, Ordering::SeqCst);
                (200, json!({"result":"reset"}).to_string())
            } else {
                (404, "{}".into())
            }
        })
    };
    let fixture = Fixture::start_claude(
        &[("CLAUDE_FIXTURE_USAGE", usage_reply().as_str()), ("T3CODE_CLAUDE_API_BASE", origin.url.as_str()), ("T3CODE_HOST_PLATFORM", "linux")],
        &[("claude-home/.credentials.json", json!({"claudeAiOauth":{"accessToken":"token-1"}}).to_string()), ("claude-home/.claude.json", json!({"oauthAccount":{"organizationUuid":"org-1"}}).to_string())],
    )
    .await;
    let (mut dom, _props, state) = open(&fixture, &SCOPES).await;
    // Limits is the first page; opening it asks the server to read the providers once.
    wait_for_rendered_text(&mut dom, "Weekly \u{b7} Fable 5").await;
    let text = rendered_text(&dom);
    assert!(text.contains("Claude") && text.contains("Session") && text.contains("90%") && text.contains("80%") && text.contains("70%"), "{text}");
    assert!(text.contains("\u{1f39f}2"), "the banked credits are on the segment: {text}");
    assert_eq!(claims.load(Ordering::SeqCst), 0);

    // A segment opens its details, where the credits and the action are.
    click_control(&mut dom, "Claude: 90% left*");
    assert!(rendered_text(&dom).contains("2 banked \u{b7} expires in 29d") || rendered_text(&dom).contains("2 reset credits banked"), "{}", rendered_text(&dom));
    assert!(!disabled(&dom, "Use reset"));
    // Declining spends nothing, and so does Escape.
    click_control(&mut dom, "Use reset");
    assert!(rendered_text(&dom).contains("Use a reset credit?") && rendered_text(&dom).contains("This redeems one credit on your account and clears the current rate-limit windows. It cannot be undone."));
    click_control(&mut dom, "Cancel reset credit");
    assert!(!rendered_text(&dom).contains("Use a reset credit?"));
    click_control(&mut dom, "Use reset");
    key_on(&mut dom, "Use a reset credit?", Key::Escape);
    assert!(!rendered_text(&dom).contains("Use a reset credit?") && claims.load(Ordering::SeqCst) == 0);

    // Confirming sends one claim; the control stays down until the server has answered.
    click_control(&mut dom, "Use reset");
    click_control(&mut dom, "Use credit");
    assert!(disabled(&dom, "Use reset"), "the control is disabled from the click");
    assert!(!rendered_text(&dom).contains("Use a reset credit?"));
    wait_for_rendered_text(&mut dom, "Reset applied. Your windows have cleared.").await;
    assert_eq!(claims.load(Ordering::SeqCst), 1, "one claim for one confirmation");
    assert!(!disabled(&dom, "Use reset"), "the server's answer re-enables it");
    assert!(state.redeem().peek().busy.is_empty());
    fixture.finish(dom).await;
}

/// A Claude thread in the seeded project, opened.
async fn claude_thread(dom: &mut VirtualDom, props: &Harness, state: Store<UiModel>, fixture: &Fixture) {
    let state_seeded = crate::runtime::diff_tests::seed(dom, props, &fixture.workspace).await;
    assert_eq!(state_seeded.peek().shell.snapshot.as_ref().unwrap().threads.len(), 2);
    dom.in_scope(ScopeId::APP, || command(&props.transport, state, "thread.create", json!({"createdBy":"user","creationSource":"web","threadId":"thread-claude","projectId":"project","title":"Claude","modelSelection":{"instanceId":"claudeAgent","model":"claude-sonnet-4-6"},"runtimeMode":"full-access","interactionMode":"default","branch":null,"worktreePath":null}))).unwrap();
    until(dom, "the Claude thread", |_| state.peek().shell.snapshot.as_ref().is_some_and(|snapshot| snapshot.threads.len() == 3)).await;
    crate::runtime::diff_tests::open_thread(dom, props, state, "thread-claude").await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn usage_limits_command_shows_the_providers_limits_from_the_menu_or_typed_and_sends_no_turn() {
    let fixture = Fixture::start_claude(&[("CLAUDE_FIXTURE_USAGE", usage_reply().as_str())], &[]).await;
    let (mut dom, props) = fixture.client(Gate::default(), &SCOPES);
    let state = props.state.borrow().unwrap();
    until(&mut dom, "the configuration", |_| state.peek().typed_config.is_some()).await;
    claude_thread(&mut dom, &props, state, &fixture).await;
    // The provider's command list gains the command, and picking it is the action itself.
    crate::runtime::transport_tests::input_control(&mut dom, "Message", "/usage");
    wait_for_rendered_text(&mut dom, "Show this provider's usage limits").await;
    key_on(&mut dom, "Message", Key::Enter);
    until(&mut dom, "the panel", |dom| control(dom, "Dismiss usage limits").is_some()).await;
    let text = rendered_text(&dom);
    assert!(text.contains("Usage limits") && text.contains("Claude") && text.contains("Session") && text.contains("90% left") && text.contains("Weekly"), "{text}");
    assert!(state.draft().peek().is_empty(), "the draft keeps nothing of the command");
    // Escape and the dismiss button close it; typing the whole command sends no message.
    click_control(&mut dom, "Dismiss usage limits");
    assert!(control(&dom, "Dismiss usage limits").is_none());
    crate::runtime::transport_tests::input_control(&mut dom, "Message", "/usage-limits");
    key_on(&mut dom, "Message", Key::Escape);
    click_control(&mut dom, "Submit message");
    until(&mut dom, "the panel", |dom| control(dom, "Dismiss usage limits").is_some()).await;
    assert!(state.draft().peek().is_empty());
    assert_eq!(crate::runtime::diff_tests::completed_runs(state), 0, "no turn was started");
    assert!(state.peek().pending_messages.is_empty());
    // The panel is for this thread: another thread does not show it.
    crate::runtime::diff_tests::open_thread(&mut dom, &props, state, "thread-a").await;
    assert!(control(&dom, "Dismiss usage limits").is_none());
    // The other thread's provider (Codex) reports its own account's limits now, so the server offers it the command without any
    // hub (`withUsageLimitsCommands`); the panel is that provider's own.
    crate::runtime::transport_tests::input_control(&mut dom, "Message", "/usage");
    wait_for_rendered_text(&mut dom, "Show this provider's usage limits").await;
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_thread_that_stopped_on_a_usage_limit_offers_recovery_and_shows_what_the_server_says() {
    let fixture = Fixture::start_claude(&[("CLAUDE_FIXTURE_SCENARIO", "limit"), ("CLAUDE_FIXTURE_USAGE", usage_reply().as_str())], &[]).await;
    let (mut dom, props) = fixture.client(Gate::default(), &SCOPES);
    let state = props.state.borrow().unwrap();
    until(&mut dom, "the configuration", |_| state.peek().typed_config.is_some()).await;
    claude_thread(&mut dom, &props, state, &fixture).await;
    assert!(!rendered_text(&dom).contains("Usage limit reached"));
    crate::runtime::transport_tests::input_control(&mut dom, "Message", "Do the work");
    dom.in_scope(ScopeId::APP, || send_message(&props.transport, state));
    wait_for_rendered_text(&mut dom, "Usage limit reached").await;
    let text = rendered_text(&dom);
    assert!(text.contains(&format!("Resets {}/", chrono::Local::now().format("%-m"))) && text.contains(&chrono::Local::now().format("%Y").to_string()), "the reset time the provider reported: {text}");
    assert!(control(&dom, "Resume at reset").is_some() && control(&dom, "Snooze until reset").is_some());
    // The choice goes to the server, whose answer is shown where the button is; the button is released by it.
    click_control(&mut dom, "Resume at reset");
    assert!(disabled(&dom, "Resume at reset"), "disabled from the click until the server has answered");
    until(&mut dom, "the server's answer", |dom| !disabled(dom, "Resume at reset")).await;
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_lost_connection_marks_what_is_shown_out_of_date_and_stops_every_write() {
    let _ = rates();
    let fixture = Fixture::start(false).await;
    write_codex_history(&fixture);
    let (mut dom, props, state) = open(&fixture, &SCOPES).await;
    click_control(&mut dom, "Cost");
    loaded_days(&mut dom, state, 30).await;
    wait_for_rendered_text(&mut dom, "gpt-5").await;
    assert!(!rendered_text(&dom).contains("The connection was lost"));
    // A read in flight when the connection drops never lands: it was for the connection that is gone.
    click_control(&mut dom, "Refresh usage");
    dom.in_scope(ScopeId::APP, || interrupt_connection(&props.transport, state, "the network went away"));
    wait_for_rendered_text(&mut dom, "The connection was lost.").await;
    until(&mut dom, "the scan to be over", |_| !state.usage().peek().scanning).await;
    assert!(rendered_text(&dom).contains("$1.30"), "the last result stays on screen");
    assert!(disabled(&dom, "Refresh usage"), "nothing can be read until the connection returns");
    assert!(state.usage().peek().error.is_none() || !rendered_text(&dom).contains("Scanning"));
    fixture.finish(dom).await;
}

const WRITER: [AuthEnvironmentScope; 4] = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::DiagnosticsRead, AuthEnvironmentScope::SettingsWrite];

fn saved_prices(state: Store<UiModel>) -> serde_json::Value {
    state.config().peek()["settings"]["usagePriceOverrides"].clone()
}
fn saved_aliases(state: Store<UiModel>) -> serde_json::Value {
    state.config().peek()["settings"]["usageModelAliases"].clone()
}

/// Custom prices and mappings are staged in the table, validated as they are typed, saved together in one patch the server stores,
/// and reset to automatic by their row.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn custom_prices_and_mappings_are_staged_validated_saved_and_reset_from_the_editor() {
    use crate::runtime::transport_tests::input_control;
    let _ = rates();
    let fixture = Fixture::start(false).await;
    write_codex_history(&fixture);
    let (mut dom, _props, state) = open(&fixture, &WRITER).await;
    click_control(&mut dom, "Cost");
    loaded_days(&mut dom, state, 30).await;
    click_control(&mut dom, "Model prices");
    until(&mut dom, "the editor", |dom| control(dom, "Add model price").is_some()).await;
    assert!(rendered_text(&dom).contains("No custom prices or mappings. Add a row to set one."));
    assert!(disabled(&dom, "Save changes"), "nothing is staged");

    // A row with a model and only one of the two required rates cannot be saved, and says which is missing and where.
    click_control(&mut dom, "Add model price");
    input_control(&mut dom, "New model ID", "gpt-9");
    input_control(&mut dom, "Input price for gpt-9", "2.5");
    assert!(rendered_text(&dom).contains("Output is required on"), "{}", rendered_text(&dom));
    assert!(disabled(&dom, "Save changes"));
    input_control(&mut dom, "Output price for gpt-9", "-1");
    assert!(rendered_text(&dom).contains("Use non-negative numbers for prices."));
    input_control(&mut dom, "Output price for gpt-9", "10");
    assert!(!disabled(&dom, "Save changes") && rendered_text(&dom).contains("Changes apply to"), "{}", rendered_text(&dom));
    // Discarding returns to the saved table.
    click_control(&mut dom, "Discard changes");
    assert!(control(&dom, "New model ID").is_none() && disabled(&dom, "Save changes"));

    // A price is saved, and its row comes from the saved settings from then on.
    click_control(&mut dom, "Add model price");
    input_control(&mut dom, "New model ID", "gpt-9");
    input_control(&mut dom, "Input price for gpt-9", "2.5");
    input_control(&mut dom, "Output price for gpt-9", "10");
    click_control(&mut dom, "Save changes");
    until(&mut dom, "the server to hold the price", |_| saved_prices(state)["gpt-9"].is_object()).await;
    assert_eq!(saved_prices(state)["gpt-9"], json!({"inputCostPerMillionTokens":2.5,"outputCostPerMillionTokens":10.0}));
    until(&mut dom, "the saved note", |dom| rendered_text(dom).contains("Saved")).await;
    assert!(control(&dom, "Reset price for gpt-9 to automatic").is_some() && control(&dom, "New model ID").is_none());
    assert_eq!(control_attribute(&dom, "Input price for gpt-9", "value").and_then(|(_, value)| value).as_deref(), Some("2.5"));

    // A mapping counts a model's usage as another's, and saving it drops the mapped model's own price.
    click_control(&mut dom, "Add model price");
    input_control(&mut dom, "New model ID", "gpt-9-preview");
    input_control(&mut dom, "Map gpt-9-preview to model", "gpt-9");
    assert!(rendered_text(&dom).contains("Counted as gpt-9"));
    click_control(&mut dom, "Save changes");
    until(&mut dom, "the server to hold the mapping", |_| saved_aliases(state)["gpt-9-preview"] == "gpt-9").await;
    assert!(saved_prices(state)["gpt-9"].is_object(), "the target keeps its price");
    until(&mut dom, "the mapped row", |dom| control(dom, "Reset price for gpt-9-preview to automatic").is_some()).await;

    // A model that already has a row cannot be added again.
    click_control(&mut dom, "Add model price");
    input_control(&mut dom, "New model ID", "gpt-9");
    assert!(rendered_text(&dom).contains("This model already has a row. Edit its prices there."));
    click_control(&mut dom, "Remove new model");

    // Resetting a row removes the custom price once saved, and can be undone before.
    click_control(&mut dom, "Reset price for gpt-9 to automatic");
    assert!(rendered_text(&dom).contains("Automatic pricing after saving"));
    click_control(&mut dom, "Undo reset for gpt-9");
    assert!(!rendered_text(&dom).contains("Automatic pricing after saving") && disabled(&dom, "Save changes"));
    click_control(&mut dom, "Reset price for gpt-9 to automatic");
    click_control(&mut dom, "Save changes");
    until(&mut dom, "the price to be gone", |_| saved_prices(state).get("gpt-9").is_none()).await;
    fixture.finish(dom).await;
}

/// A reader can look but not save: the save is refused with the reason, and the edits are kept for a retry.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_connection_without_the_settings_grant_cannot_save_prices_and_keeps_the_edits() {
    use crate::runtime::transport_tests::input_control;
    let _ = rates();
    let fixture = Fixture::start(false).await;
    write_codex_history(&fixture);
    let (mut dom, _props, state) = open(&fixture, &SCOPES).await;
    click_control(&mut dom, "Model prices");
    until(&mut dom, "the editor", |dom| control(dom, "Add model price").is_some()).await;
    assert!(rendered_text(&dom).contains("Read-only access"));
    click_control(&mut dom, "Add model price");
    input_control(&mut dom, "New model ID", "gpt-9");
    input_control(&mut dom, "Input price for gpt-9", "1");
    input_control(&mut dom, "Output price for gpt-9", "2");
    click_control(&mut dom, "Save changes");
    until(&mut dom, "the refusal", |dom| rendered_text(dom).contains("Not saved \u{b7} Read-only access")).await;
    assert!(control(&dom, "Retry failed saves").is_some() && disabled(&dom, "Input price for gpt-9"), "the table is locked until the failure is dealt with");
    assert!(saved_prices(state).get("gpt-9").is_none());
    click_control(&mut dom, "Discard pending changes");
    assert!(control(&dom, "New model ID").is_none() && !rendered_text(&dom).contains("Not saved"));
    fixture.finish(dom).await;
}

/// A model with usage and no known price offers to be priced from its own dialog, in a row already started for it.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_model_without_a_price_offers_set_price_with_its_row_started() {
    let _ = rates();
    let fixture = Fixture::start(false).await;
    write_codex_history_of(&fixture, "mystery-model");
    let (mut dom, _props, state) = open(&fixture, &WRITER).await;
    click_control(&mut dom, "Cost");
    loaded_days(&mut dom, state, 30).await;
    wait_for_rendered_text(&mut dom, "mystery-model").await;
    click_control(&mut dom, "Open mystery-model");
    until(&mut dom, "the model's dialog", |dom| control(dom, "Set price").is_some()).await;
    click_control(&mut dom, "Set price");
    until(&mut dom, "the editor", |dom| control(dom, "Input price for mystery-model").is_some()).await;
    assert!(control(&dom, "Set price").is_none(), "the model's dialog closed");
    assert_eq!(control_attribute(&dom, "New model ID", "value").and_then(|(_, value)| value).as_deref(), Some("mystery-model"));
    fixture.finish(dom).await;
}
