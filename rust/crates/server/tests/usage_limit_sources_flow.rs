//! Usage-limit sources (`UsageLimitSources.ts`, `cliproxyApi.ts`): the hub client against what the unchanged original made of the
//! same scripted hubs (`tools/generate_usage_limit_source_fixtures.mjs`), the Codex mapper and the `/usage-limits` command rows
//! the same run recorded, then the service (polling, settings changes, redemption) and the RPCs over a real socket. The only
//! network is loopback: a scripted hub on a socket of the test, or a fake in memory.
mod rpc_support;
use async_trait::async_trait;
use axum::{
    Router,
    body::Bytes,
    http::{HeaderMap, Method, StatusCode, Uri},
    response::IntoResponse,
};
use rpc_support::{defect, failure, harness_with_config, next, rpc, send, success};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, AtomicUsize, Ordering},
    },
};
use t3_server::{
    cliproxy_api::{Clock, CliproxyApi, Hub, HubConfig, SourceError},
    codex_usage_limits,
    server_settings::{SettingsOptions, SettingsService},
    test_support::HANG_BOUND,
    usage_limit_sources::{self, UsageLimitSources, same_command_coverage, with_usage_limits_commands},
};

fn rows(kind: &str) -> Vec<Value> {
    include_str!("fixtures/usage-limit-sources.jsonl")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row["kind"] == kind)
        .collect()
}

// ---- the scripted hub, served from a loopback socket -------------------------------------------

struct ScriptedHub {
    address: std::net::SocketAddr,
    seen: Arc<Mutex<Vec<Value>>>,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for ScriptedHub {
    fn drop(&mut self) {
        self.server.abort();
    }
}
impl ScriptedHub {
    async fn start(scenario: Value) -> Self {
        let seen = Arc::new(Mutex::new(vec![]));
        let (script, record) = (Arc::new(scenario), seen.clone());
        let router = Router::new().fallback(move |method: Method, uri: Uri, headers: HeaderMap, body: Bytes| {
            let (script, record) = (script.clone(), record.clone());
            async move { respond(&script, &record, method, uri, headers, body) }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self { address, seen, server }
    }
    fn config(&self) -> HubConfig {
        HubConfig { url: format!("http://{}", self.address), management_key: "management-secret".into() }
    }
}

fn reply(value: Value, status: u16) -> axum::response::Response {
    (StatusCode::from_u16(status).unwrap(), axum::Json(value)).into_response()
}

/// The hub of the generator's script: the account list, the cooldown reset, and an `api-call` answered by the entry for the
/// account's index and the URL it asked the hub to call.
fn respond(script: &Value, record: &Mutex<Vec<Value>>, method: Method, uri: Uri, headers: HeaderMap, body: Bytes) -> axum::response::Response {
    assert_eq!(headers["authorization"], "Bearer management-secret");
    let path = uri.path().to_owned();
    let body: Option<Value> = (!body.is_empty()).then(|| serde_json::from_slice(&body).unwrap());
    let mut seen = json!({"method":method.as_str(),"path":path});
    if let Some(body) = &body {
        let mut body = body.clone();
        if let Some(data) = body.get("data").and_then(Value::as_str).map(|data| serde_json::from_str::<Value>(data).unwrap()) {
            body["data"] = data;
        }
        seen["body"] = body;
    }
    record.lock().unwrap().push(seen);
    match path.as_str() {
        "/v0/management/auth-files" => match script["authStatus"].as_u64() {
            Some(status) => reply(json!({}), status as u16),
            None => reply(if script["authFilesBody"].is_null() { json!({"files":script["files"]}) } else { script["authFilesBody"].clone() }, 200),
        },
        "/v0/management/reset-quota" => reply(json!({}), script["cooldownStatus"].as_u64().unwrap_or(200) as u16),
        "/v0/management/api-call" => {
            let body = body.unwrap();
            let key = format!("{}|{}", body["auth_index"].as_str().unwrap(), body["url"].as_str().unwrap());
            let answer = &script["upstream"][&key];
            if answer.is_null() {
                return reply(json!({}), 500);
            }
            if let Some(status) = answer["http"].as_u64() {
                return reply(json!({}), status as u16);
            }
            if !answer["envelope"].is_null() {
                return reply(answer["envelope"].clone(), 200);
            }
            let text = answer["body"].as_str().map_or_else(|| answer["body"].to_string(), str::to_owned);
            reply(json!({"status_code":answer["status"],"body":text}), 200)
        }
        _ => reply(json!({}), 404),
    }
}

/// Requests in an order that does not depend on how concurrent reads interleaved or on the order of keys.
fn canonical(requests: &[Value]) -> Vec<Value> {
    fn sorted(value: &Value) -> String {
        match value {
            Value::Object(map) => {
                let mut entries: Vec<_> = map.iter().collect();
                entries.sort_by_key(|(key, _)| key.as_str());
                format!("{{{}}}", entries.into_iter().map(|(key, value)| format!("{key:?}:{}", sorted(value))).collect::<Vec<_>>().join(","))
            }
            Value::Array(items) => format!("[{}]", items.iter().map(sorted).collect::<Vec<_>>().join(",")),
            other => other.to_string(),
        }
    }
    let mut requests = requests.to_vec();
    requests.sort_by_key(sorted);
    requests
}

fn fixed(now: i64) -> Clock {
    Arc::new(move || now)
}

#[tokio::test]
async fn the_hub_client_reads_and_redeems_as_the_original_does() {
    let mut count = 0;
    for row in rows("hub") {
        let name = row["name"].as_str().unwrap();
        let hub = ScriptedHub::start(row["input"].clone()).await;
        let (api, config, clock) = (CliproxyApi::default(), hub.config(), fixed(row["now"].as_i64().unwrap()));
        let source = |result: Result<Value, SourceError>, field: &str| match result {
            Ok(value) => json!({ field: value }),
            Err(error) => json!({"error":error.0}),
        };
        let output = match row["input"]["op"].as_str().unwrap() {
            "read" => source(api.read_accounts(&config, &clock).await.map(Value::from), "accounts"),
            "consume" => source(api.consume(&config, row["input"]["accountId"].as_str().unwrap(), row["input"]["creditId"].as_str().unwrap(), &clock).await, "result"),
            "consumeMany" => {
                let mut results = vec![];
                for attempt in row["input"]["attempts"].as_array().unwrap() {
                    results.push(source(api.consume(&config, attempt[0].as_str().unwrap(), attempt[1].as_str().unwrap(), &clock).await, "result"));
                }
                json!({"results":results})
            }
            other => panic!("{other}"),
        };
        assert_eq!(output, row["output"], "{name}");
        assert_eq!(canonical(&hub.seen.lock().unwrap()), canonical(row["requests"].as_array().unwrap()), "{name}: requests");
        count += 1;
    }
    assert!(count >= 25, "{count} hub scenarios");
}

#[test]
fn codex_limits_plans_and_the_usage_command_match_the_original() {
    let (mut limits, mut updates, mut plans, mut commands, mut coverage) = (0, 0, 0, 0, 0);
    for row in rows("codexLimits") {
        let input = &row["input"];
        let output = codex_usage_limits::rate_limits_to_limits(&input["snapshot"], input.get("rateLimitsByLimitId"), input.get("resetCredits"), input["checkedAt"].as_str().unwrap());
        assert_eq!(output, row["output"], "{input}");
        limits += 1;
    }
    for row in rows("codexUpdate") {
        assert_eq!(codex_usage_limits::rate_limits_to_update(&row["snapshot"]).unwrap_or(Value::Null), row["output"], "{}", row["snapshot"]);
        updates += 1;
    }
    for row in rows("planLabel") {
        assert_eq!(codex_usage_limits::plan_label(row["plan"].as_str()).map_or(Value::Null, |label| json!(label)), row["output"], "{}", row["plan"]);
        plans += 1;
    }
    for row in rows("commands") {
        let output = with_usage_limits_commands(row["providers"].as_array().unwrap(), row["sources"].as_array().unwrap());
        assert_eq!(json!(output), row["output"], "{} / {}", row["providers"], row["sources"]);
        commands += 1;
    }
    for row in rows("coverage") {
        assert_eq!(same_command_coverage(row["previous"].as_array().unwrap(), row["next"].as_array().unwrap()), row["output"], "{} -> {}", row["previous"], row["next"]);
        coverage += 1;
    }
    assert!(limits >= 200 && updates >= 10 && plans >= 20 && commands >= 10 && coverage >= 30, "{limits} {updates} {plans} {commands} {coverage}");
}

// ---- the service -----------------------------------------------------------------------------

/// A hub in memory: the accounts each URL has (or its failure), and every read and redemption counted.
#[derive(Default)]
struct FakeHub {
    accounts: Mutex<HashMap<String, Result<Vec<Value>, String>>>,
    reads: AtomicUsize,
    consumed: Mutex<Vec<(String, String, String)>>,
    outcome: Mutex<Option<Result<Value, String>>>,
}
#[async_trait]
impl Hub for FakeHub {
    async fn read_accounts(&self, config: &HubConfig, _clock: &Clock) -> Result<Vec<Value>, SourceError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.accounts.lock().unwrap().get(&config.url).cloned().unwrap_or(Ok(vec![])).map_err(SourceError)
    }
    async fn consume(&self, config: &HubConfig, account_id: &str, credit_id: &str, _clock: &Clock) -> Result<Value, SourceError> {
        self.consumed.lock().unwrap().push((config.url.clone(), account_id.into(), credit_id.into()));
        self.outcome.lock().unwrap().clone().unwrap_or(Ok(json!({"outcome":"reset"}))).map_err(SourceError)
    }
}
impl FakeHub {
    fn serve(&self, url: &str, answer: Result<Vec<Value>, &str>) {
        self.accounts.lock().unwrap().insert(url.into(), answer.map_err(str::to_owned));
    }
}

fn account(id: &str, driver: &str) -> Value {
    json!({"id":id,"driver":driver,"usageLimits":{"checkedAt":"2026-09-06T16:00:00.000Z","windows":[]}})
}
fn source_entry(url: &str, key: &str) -> Value {
    json!({"kind":"cliproxy","url":url,"managementKey":key})
}
/// A clock that moves on each reading, so every read of a source is a different snapshot.
fn ticking() -> Clock {
    let now = Arc::new(AtomicI64::new(1_788_710_400_000));
    Arc::new(move || now.fetch_add(1, Ordering::SeqCst))
}

async fn settings_with(directory: &std::path::Path, document: Value) -> SettingsService {
    let path = directory.join("settings.json");
    std::fs::write(&path, document.to_string()).unwrap();
    let mut options = SettingsOptions::file(path, t3_server::server_secret_store::ServerSecretStore::open(directory.join("secrets")).unwrap());
    options.watch = false;
    SettingsService::start(options).await.unwrap()
}
async fn patch(settings: &SettingsService, value: Value) {
    settings.update(serde_json::from_value(value).unwrap()).await.unwrap();
}

/// The next published set that satisfies `wanted`.
async fn until(changes: &mut usage_limit_sources::SourceChanges, wanted: impl Fn(&[Value]) -> bool) -> Arc<Vec<Value>> {
    tokio::time::timeout(HANG_BOUND, async {
        loop {
            let set = changes.next().await.expect("the service is running");
            if wanted(&set) {
                return set;
            }
        }
    })
    .await
    .expect("the awaited set was published")
}

#[tokio::test]
async fn sources_follow_the_settings_and_keep_a_failed_row() {
    let directory = tempfile::tempdir().unwrap();
    let settings = settings_with(directory.path(), json!({"usageLimitSources":{"hub":source_entry("http://hub.test:8317", "key")}})).await;
    let hub = Arc::new(FakeHub::default());
    hub.serve("http://hub.test:8317", Ok(vec![account("a.json", "codex")]));
    hub.serve("http://down.test", Err("The hub could not list accounts."));
    let sources = UsageLimitSources::start(Some(settings.clone()), None, hub.clone(), ticking()).await;
    let mut changes = sources.subscribe();
    let first = until(&mut changes, |set| set.len() == 1 && !set[0]["accounts"].as_array().unwrap().is_empty()).await;
    // The label falls back to the hub's host, with its port.
    assert_eq!((first[0]["id"].as_str(), first[0]["label"].as_str(), first[0]["kind"].as_str()), (Some("hub"), Some("hub.test:8317"), Some("cliproxy")));
    assert_eq!(first[0]["accounts"], json!([account("a.json", "codex")]));

    // A new entry is read straight away, in the entries' order; a failing hub keeps its row, with the error and no accounts.
    patch(&settings, json!({"usageLimitSources":{"down":{"kind":"cliproxy","label":"Down","url":"http://down.test","managementKey":"key"},"keyless":{"kind":"cliproxy","url":"not a url","managementKey":""},"off":{"kind":"cliproxy","url":"http://off.test","managementKey":"key","enabled":false}}})).await;
    let more = until(&mut changes, |set| set.len() == 3).await;
    let by_id = |id: &str| more.iter().find(|source| source["id"] == id).unwrap().clone();
    assert_eq!(by_id("down")["label"], "Down");
    assert_eq!((by_id("down")["accounts"].clone(), by_id("down")["error"].clone()), (json!([]), json!("The hub could not list accounts.")));
    // An unusable URL falls back to the entry's id, and an empty key is not sent anywhere.
    assert_eq!((by_id("keyless")["label"].clone(), by_id("keyless")["error"].clone()), (json!("keyless"), json!("No management key configured.")));
    assert!(more.iter().all(|source| source["id"] != "off"), "a disabled entry has no row");

    // A removed entry leaves the list, and a read in flight from before cannot bring it back.
    patch(&settings, json!({"usageLimitSources":{"hub":null,"down":null,"keyless":null}})).await;
    let removed = until(&mut changes, |set| set.iter().all(|source| source["id"] == "off") && set.is_empty()).await;
    assert!(removed.is_empty());
    assert!(sources.current().is_empty());
    settings.shutdown().await;
}

#[tokio::test]
async fn sources_are_read_again_on_the_provider_health_interval() {
    let directory = tempfile::tempdir().unwrap();
    let settings = settings_with(directory.path(), json!({"usageLimitSources":{"hub":source_entry("http://hub.test", "key")}})).await;
    let hub = Arc::new(FakeHub::default());
    hub.serve("http://hub.test", Ok(vec![account("a.json", "codex")]));
    // The wait of the poll is a tick the test gives, so no time passes in the test and the interval it asked for is what is recorded.
    let ticks = Arc::new(tokio::sync::Semaphore::new(0));
    let waits = Arc::new(Mutex::new(vec![]));
    let sleeper: usage_limit_sources::Sleeper = {
        let (ticks, waits) = (ticks.clone(), waits.clone());
        Arc::new(move |wait| {
            let (ticks, waits) = (ticks.clone(), waits.clone());
            Box::pin(async move {
                waits.lock().unwrap().push(wait);
                ticks.acquire().await.unwrap().forget();
            })
        })
    };
    let sources = UsageLimitSources::start_with(Some(settings.clone()), None, hub.clone(), ticking(), sleeper).await;
    let mut changes = sources.subscribe();
    let first = until(&mut changes, |set| set.len() == 1).await[0]["checkedAt"].clone();
    ticks.add_permits(1);
    let second = until(&mut changes, |set| set.len() == 1 && set[0]["checkedAt"] != first).await[0]["checkedAt"].clone();
    ticks.add_permits(1);
    let third = until(&mut changes, |set| set.len() == 1 && set[0]["checkedAt"] != second).await[0]["checkedAt"].clone();
    assert_ne!(first, third);
    // `balanced`, the default profile, reads providers every five minutes.
    assert!(waits.lock().unwrap().iter().all(|wait| *wait == std::time::Duration::from_secs(300)), "{:?}", waits.lock().unwrap());
    settings.shutdown().await;
}

#[tokio::test]
async fn a_redemption_reads_that_source_again_and_the_others_stay() {
    let directory = tempfile::tempdir().unwrap();
    let settings = settings_with(
        directory.path(),
        json!({"usageLimitSources":{"one":source_entry("http://one.test", "key"),"two":source_entry("http://two.test", "key"),"keyless":{"kind":"cliproxy","url":"http://k.test","managementKey":""},"off":{"kind":"cliproxy","url":"http://off.test","managementKey":"key","enabled":false}}}),
    )
    .await;
    let hub = Arc::new(FakeHub::default());
    hub.serve("http://one.test", Ok(vec![account("a.json", "codex")]));
    hub.serve("http://two.test", Ok(vec![account("b.json", "codex")]));
    let sources = UsageLimitSources::start(Some(settings.clone()), None, hub.clone(), ticking()).await;
    let mut changes = sources.subscribe();
    let before = until(&mut changes, |set| set.len() == 3 && set.iter().all(|source| source["error"].is_null() || source["id"] == "keyless")).await;

    // The hub now reports the account spent; only source one is read again.
    hub.serve("http://one.test", Ok(vec![]));
    let result = sources.consume_reset_credit("one", "a.json", "credit-1").await.unwrap();
    assert_eq!(result, json!({"outcome":"reset"}));
    assert_eq!(hub.consumed.lock().unwrap().as_slice(), [("http://one.test".to_owned(), "a.json".to_owned(), "credit-1".to_owned())]);
    let after = sources.current();
    let find = |set: &[Value], id: &str| set.iter().find(|source| source["id"] == id).unwrap().clone();
    assert_eq!(find(&after, "one")["accounts"], json!([]));
    assert_eq!(find(&after, "two"), find(&before, "two"), "the others are not read again");

    // A hub's refusal reaches the caller as it said it, and reads nothing.
    *hub.outcome.lock().unwrap() = Some(Err("The provider refused the hub request (HTTP 429).".into()));
    let refused = sources.consume_reset_credit("one", "a.json", "credit-1").await.unwrap_err();
    assert_eq!(refused.0, "The provider refused the hub request (HTTP 429).");

    // Missing, disabled and keyless sources are all the same sentence, and the hub is not called for them.
    let calls = hub.consumed.lock().unwrap().len();
    for id in ["nope", "off", "keyless"] {
        let error = sources.consume_reset_credit(id, "a.json", "c").await.unwrap_err();
        assert_eq!(error.0, "The usage limit source is missing or disabled.", "{id}");
    }
    assert_eq!(hub.consumed.lock().unwrap().len(), calls);
    settings.shutdown().await;
}

// ---- over a socket -----------------------------------------------------------------------------

#[tokio::test]
async fn the_server_streams_sources_redeems_through_the_hub_and_offers_the_command() {
    let hub = ScriptedHub::start(json!({
        "files": [{"id":"a.json","auth_index":"a","provider":"codex","email":"a@example.com","id_token":{"chatgpt_account_id":"acct-a"}}],
        "upstream": {
            "a|https://chatgpt.com/backend-api/wham/usage": {"status":200,"body":{"plan_type":"pro","rate_limit":{"primary_window":{"used_percent":12.5,"reset_at":1800000000,"limit_window_seconds":18000}}}},
            "a|https://chatgpt.com/backend-api/wham/rate-limit-reset-credits": {"status":200,"body":{"credits":[{"id":"first","status":"available","reset_type":"codex_rate_limits","expires_at":"2099-01-01T00:00:00Z"}]}},
            "a|https://chatgpt.com/backend-api/wham/rate-limit-reset-credits/consume": {"status":200,"body":{"code":"reset"}},
        },
    }))
    .await;
    let h = harness_with_config().await;
    let settings = h.settings.clone().unwrap();
    let sources = usage_limit_sources::start_production(Some(settings), None).await;
    usage_limit_sources::install(&h.store, sources.clone());
    // Spending a credit and refreshing providers are provider management, which the harness' operator is not granted.
    let manager = h.auth.issue_session("fixture", "bearer-access-token", vec![t3_contracts::AuthEnvironmentScope::OrchestrationRead, t3_contracts::AuthEnvironmentScope::OrchestrationOperate, t3_contracts::AuthEnvironmentScope::ProvidersManage, t3_contracts::AuthEnvironmentScope::SettingsWrite], json!({"deviceType":"unknown"}), chrono::Utc::now(), chrono::Duration::hours(1)).unwrap().1;
    let mut operator = h.connect(&manager).await;

    // The quota is never in the snapshot; a client that asked is sent the set as it is, then each change, and `/usage-limits` is
    // offered on the providers that report limits (none yet).
    let mut watcher = h.connect(&h.operator.clone()).await;
    send(&mut watcher, json!({"_tag":"Request","id":2,"tag":"subscribeServerConfig","payload":{"usageLimitSources":true,"usageLimitsCommand":true},"headers":[]})).await;
    let first = event_where(&mut watcher, 2, |value| value["type"] == "snapshot").await;
    assert!(first["config"].get("usageLimitSources").is_none());
    let offered = |providers: &Value| -> Vec<String> { providers.as_array().unwrap().iter().filter(|provider| provider["slashCommands"].as_array().unwrap().iter().any(|command| command["name"] == "usage-limits")).map(|provider| provider["driver"].as_str().unwrap().to_owned()).collect() };
    assert!(offered(&first["config"]["providers"]).is_empty(), "no source, no command");
    let initial = event_where(&mut watcher, 2, |value| value["type"] == "usageLimitSourcesUpdated").await;
    assert_eq!(initial["payload"]["sources"], json!([]));

    // Adding the source through settings (its key becomes a secret the server keeps) is read straight away.
    let update = rpc(&mut operator, 3, "server.updateSettings", json!({"patch":{"usageLimitSources":{"hub":{"kind":"cliproxy","label":"Proxy","url":format!("http://{}", hub.address),"managementKey":"management-secret"}}}})).await;
    success(&update);
    let (mut shown, mut catalog) = (None, None);
    while shown.is_none() || catalog.is_none() {
        for event in chunk_values(&mut watcher, 2).await {
            match event["type"].as_str() {
                Some("usageLimitSourcesUpdated") if event["payload"]["sources"][0]["accounts"].as_array().is_some_and(|accounts| !accounts.is_empty()) => shown = Some(event["payload"]["sources"][0].clone()),
                Some("providerStatuses") if !offered(&event["payload"]["providers"]).is_empty() => catalog = Some(event["payload"]["providers"].clone()),
                _ => {}
            }
        }
    }
    let shown = shown.unwrap();
    assert_eq!((shown["id"].as_str(), shown["label"].as_str()), (Some("hub"), Some("Proxy")));
    assert_eq!(shown["accounts"][0]["plan"], "ChatGPT Pro 20x Subscription");
    assert_eq!(shown["accounts"][0]["usageLimits"]["resetCredits"]["nextCreditId"], "first");
    // The command is offered on the providers the source reports on (a codex account: the codex provider).
    assert_eq!(offered(&catalog.unwrap()), ["codex"]);

    // Redeeming through the hub: the reset, and the source read again.
    let spent = rpc(&mut operator, 4, "provider.consumeResetCredit", json!({"sourceId":"hub","accountId":"a.json","creditId":"first"})).await;
    assert_eq!(success(&spent), &json!({"outcome":"reset"}));
    let calls: Vec<String> = hub.seen.lock().unwrap().iter().filter(|seen| seen["body"]["data"]["credit_id"] == "first").map(|seen| seen["body"]["url"].as_str().unwrap().to_owned()).collect();
    assert_eq!(calls, ["https://chatgpt.com/backend-api/wham/rate-limit-reset-credits/consume"]);
    assert!(hub.seen.lock().unwrap().iter().any(|seen| seen["path"] == "/v0/management/reset-quota"), "the hub's cooldown is cleared");
    let missing = rpc(&mut operator, 5, "provider.consumeResetCredit", json!({"sourceId":"nope","accountId":"a.json","creditId":"first"})).await;
    assert_eq!(failure(&missing), &json!({"_tag":"UsageLimitSourceError","detail":"The usage limit source is missing or disabled."}));
    let blank = rpc(&mut operator, 6, "provider.consumeResetCredit", json!({"sourceId":"hub","accountId":"  ","creditId":"first"})).await;
    assert!(defect(&blank).as_str().unwrap().contains("accountId"), "{blank}");

    // An untargeted provider refresh re-reads the sources first; a targeted one does not.
    let reads = || hub.seen.lock().unwrap().iter().filter(|seen| seen["path"] == "/v0/management/auth-files").count();
    let before = reads();
    success(&rpc(&mut operator, 7, "server.refreshProviders", json!({"instanceId":"codex"})).await);
    assert_eq!(reads(), before);
    success(&rpc(&mut operator, 8, "server.refreshProviders", json!({})).await);
    assert_eq!(reads(), before + 1);
    h.finish().await;
}

/// The values of the next stream chunk, acknowledged.
async fn chunk_values(socket: &mut rpc_support::Socket, id: u64) -> Vec<Value> {
    tokio::time::timeout(HANG_BOUND, async {
        loop {
            let message = next(socket).await;
            assert_ne!(message["_tag"], "Exit", "stream ended: {message}");
            if message["_tag"] == "Chunk" {
                send(socket, json!({"_tag":"Ack","requestId":id})).await;
                return message["values"].as_array().unwrap().clone();
            }
        }
    })
    .await
    .expect("a chunk arrived")
}

/// The first value of a stream that satisfies `wanted`, each chunk acknowledged.
async fn event_where(socket: &mut rpc_support::Socket, id: u64, wanted: impl Fn(&Value) -> bool) -> Value {
    loop {
        for value in chunk_values(socket, id).await {
            if wanted(&value) {
                return value;
            }
        }
    }
}
