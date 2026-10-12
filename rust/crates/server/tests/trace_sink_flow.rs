//! A browser's span export, over a real socket: it needs the operate scope, lands in the local trace file the diagnostics read, and
//! is answered 204 (`/api/observability/v1/traces`).
mod rpc_support;
use rpc_support::harness_with_config;
use serde_json::{Value, json};
use t3_server::trace_sink::{OTLP_TRACES_PROXY_PATH, TraceSink};

fn export(name: &str) -> Value {
    json!({"resourceSpans":[{"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"web"}}]},"scopeSpans":[{"scope":{"name":"browser"},"spans":[{"traceId":"0af7651916cd43dd8448eb211c80319c","spanId":"b7ad6b7169203331","name":name,"kind":1,"startTimeUnixNano":"1","endTimeUnixNano":"2000001","attributes":[],"events":[],"links":[],"status":{"code":1}}]}]}]})
}

#[tokio::test]
async fn a_browser_export_is_recorded_and_the_scope_is_checked() {
    let h = harness_with_config().await;
    let path = h.base.join("userdata/logs/server.trace.ndjson");
    let sink = TraceSink::open(&path, 1 << 20, 3).unwrap();
    t3_server::trace_sink::install(&h.store, sink.clone());
    let client = reqwest::Client::new();
    let post = |token: Option<&str>, body: String| {
        let mut request = client.post(format!("http://{}{OTLP_TRACES_PROXY_PATH}", h.address)).header("content-type", "application/json").body(body);
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        request.send()
    };
    // No credential, and a read-only one, record nothing.
    assert_eq!(post(None, export("anonymous").to_string()).await.unwrap().status(), 401);
    assert_eq!(post(Some(&h.reader), export("reader").to_string()).await.unwrap().status(), 403);
    // An operator's export is recorded and answered 204; so is one that does not decode, which is logged and dropped.
    assert_eq!(post(Some(&h.operator), export("first").to_string()).await.unwrap().status(), 204);
    assert_eq!(post(Some(&h.operator), json!({"resourceSpans":[{}]}).to_string()).await.unwrap().status(), 204);
    assert_eq!(post(Some(&h.operator), "not json".into()).await.unwrap().status(), 400);
    sink.flush();
    let written: Vec<Value> = std::fs::read_to_string(&path).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(written.len(), 1);
    assert_eq!((written[0]["type"].as_str(), written[0]["name"].as_str(), written[0]["durationMs"].clone()), (Some("otlp-span"), Some("first"), json!(2)));
    // The server config says the trace is written.
    let mut socket = h.connect(&h.operator).await;
    let config = rpc_support::rpc(&mut socket, 1, "server.getConfig", json!({})).await;
    assert_eq!(rpc_support::success(&config)["observability"]["localTracingEnabled"], true);
    h.finish().await;
}

/// The server's own spans: each RPC is a `ws.rpc.<method>` span with the original's attributes, a refused call is a failed one, a
/// subscription lasts until the client lets go of it, and the four methods the original does not trace record nothing.
#[tokio::test]
async fn rpc_calls_are_spans_in_the_local_trace_file() {
    let h = harness_with_config().await;
    let path = h.base.join("userdata/logs/server.trace.ndjson");
    let sink = TraceSink::open(&path, 1 << 20, 3).unwrap();
    t3_server::trace_sink::install(&h.store, sink.clone());
    let mut operator = h.connect(&h.operator).await;
    let config = rpc_support::rpc(&mut operator, 1, "server.getConfig", json!({})).await;
    assert_eq!(rpc_support::success(&config)["observability"]["localTracingEnabled"], true);
    rpc_support::rpc(&mut operator, 2, "server.getTraceDiagnostics", json!({})).await;
    rpc_support::rpc(&mut operator, 3, "no.such.method", json!({})).await;
    // A reader may not run what needs the operate scope: refused, and still a span.
    let mut reader = h.connect(&h.reader).await;
    let refused = rpc_support::rpc(&mut reader, 4, "server.updateSettings", json!({"patch":{}})).await;
    assert_eq!(refused["_tag"], "Failure", "{refused}");
    // A subscription's span lasts until it ends; letting go of it is an interruption.
    rpc_support::send(&mut operator, json!({"_tag":"Request","id":5,"tag":"subscribeServerLifecycle","payload":{},"headers":[]})).await;
    rpc_support::send(&mut operator, json!({"_tag":"Interrupt","requestId":5})).await;
    // The interrupt is answered, so the server has dropped the subscription (and its span) by then.
    tokio::time::timeout(t3_server::test_support::HANG_BOUND, async {
    loop {
        let message = rpc_support::next(&mut operator).await;
        if message["_tag"] == "Exit" && message["requestId"] == 5 {
            break;
        }
    }
    })
    .await
    .expect("the interrupt is answered");
    sink.flush();
    let spans: Vec<Value> = std::fs::read_to_string(&path).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    let named = |name: &str| spans.iter().filter(|span| span["name"] == name).collect::<Vec<_>>();
    let config = named("ws.rpc.server.getConfig");
    assert_eq!(config.len(), 1, "{spans:?}");
    assert_eq!((config[0]["type"].as_str(), config[0]["kind"].as_str(), config[0]["sampled"].as_bool(), config[0]["exit"]["_tag"].as_str(), config[0]["traceId"].as_str().map(str::len), config[0]["spanId"].as_str().map(str::len)), (Some("effect-span"), Some("internal"), Some(true), Some("Success"), Some(32), Some(16)));
    assert!(config[0].get("parentSpanId").is_none());
    assert_eq!(config[0]["attributes"], json!({"rpc.transport":"websocket","rpc.system":"effect-rpc","rpc.method":"server.getConfig","rpc.aggregate":"server"}));
    let started: u128 = config[0]["startTimeUnixNano"].as_str().unwrap().parse().unwrap();
    let ended: u128 = config[0]["endTimeUnixNano"].as_str().unwrap().parse().unwrap();
    assert!(ended >= started && config[0]["durationMs"].as_f64().unwrap() >= 0.0);
    assert!(named("ws.rpc.server.getTraceDiagnostics").is_empty(), "the original traces no diagnostics read");
    assert!(spans.iter().all(|span| span["name"] != "ws.rpc.no.such.method"));
    let rejected = named("ws.rpc.server.updateSettings");
    assert_eq!((rejected.len(), rejected[0]["exit"]["_tag"].as_str()), (1, Some("Failure")), "{spans:?}");
    let lifecycle = named("ws.rpc.subscribeServerLifecycle");
    assert_eq!((lifecycle.len(), lifecycle[0]["exit"]["_tag"].as_str(), lifecycle[0]["attributes"]["rpc.aggregate"].as_str()), (1, Some("Interrupted"), Some("server")), "{spans:?}");
    h.finish().await;
}
