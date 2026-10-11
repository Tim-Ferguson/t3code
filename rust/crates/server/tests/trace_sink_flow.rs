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
