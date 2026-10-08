//! Real native fast-path processes against an isolated HTTP peer.
use axum::{
    body::{Body, Bytes, to_bytes},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
};
#[derive(Clone, Default)]
struct Peer {
    records: Arc<Mutex<Vec<(HeaderMap, String)>>>,
    reverse: Arc<tokio::sync::Notify>,
    held: Arc<tokio::sync::Notify>,
    dropped: Arc<tokio::sync::Notify>,
}
async fn handle(
    axum::extract::State(peer): axum::extract::State<Peer>,
    request: axum::extract::Request,
) -> axum::response::Response {
    let (parts, body) = request.into_parts();
    let bytes = to_bytes(body, usize::MAX).await.unwrap();
    let raw = String::from_utf8(bytes.to_vec()).unwrap();
    let message: Value = serde_json::from_str(&raw).unwrap();
    let array = parts
        .headers
        .get("authorization")
        .is_some_and(|value| value == "Bearer array");
    peer.records.lock().unwrap().push((parts.headers, raw));
    match message["method"].as_str() {
        Some("initialize")=>{
            if message["id"]=="t3-acp-cli-initialize" {return ([("content-type","application/json"),("mcp-session-id","call-session")],if array {json!([{ "jsonrpc":"2.0","id":message["id"],"result":{"protocolVersion":"custom-version"}}]).to_string()} else {json!({"jsonrpc":"2.0","id":message["id"],"result":{"protocolVersion":"custom-version"}}).to_string()}).into_response();}
            let reverse=peer.reverse.clone();
            let stream=futures_util::stream::unfold(0,move|stage|{let reverse=reverse.clone();async move {match stage {
                0=>Some((Ok::<_,std::io::Error>(Bytes::from_static(b"data: {\"jsonrpc\":\"2.0\",\"id\":\"reverse\",\"method\":\"opaque/request\",\"unknown\":true}\r\n\r\n")),1)),
                1=>{reverse.notified().await;Some((Ok(Bytes::from("data: [{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"custom-version\",\"text\":\"👩‍💻中文\"},\"unknown\":{\"kept\":true}}]\n\n")),2))},
                _=>None,
            }}});
            ([("content-type","text/event-stream"),("mcp-session-id","stdio-session")],Body::from_stream(stream)).into_response()
        },
        Some("notifications/initialized")=>(StatusCode::ACCEPTED,"not JSON, intentionally ignored").into_response(),
        Some("held") => {
            struct Dropped(Arc<tokio::sync::Notify>);
            impl Drop for Dropped {fn drop(&mut self){self.0.notify_one();}}
            let held=peer.held.clone();
            let stream=futures_util::stream::unfold((false,Dropped(peer.dropped.clone())),move|(sent,guard)|{
                let held=held.clone();async move {
                    if sent {std::future::pending::<()>().await;}
                    held.notify_one();
                    Some((Ok::<_,std::io::Error>(Bytes::from_static(b"data: {")),(true,guard)))
                }
            });
            ([("content-type","text/event-stream")],Body::from_stream(stream)).into_response()
        },
        Some("quiet")=>(StatusCode::NO_CONTENT,Body::empty()).into_response(),
        Some("fail")=>(StatusCode::FORBIDDEN,"private failure detail must never be forwarded").into_response(),
        Some("tools/call")=>([("content-type","application/json")],json!({"jsonrpc":"2.0","id":message["id"],"result":{"opaque":["tool",message["params"]["arguments"]]}}).to_string()).into_response(),
        _ if message["id"]=="reverse"=>{peer.reverse.notify_one();StatusCode::ACCEPTED.into_response()},
        _=>([("content-type","application/json")],json!([{"jsonrpc":"2.0","id":message["id"],"result":{"unknown":{"preserved":true}}},{"jsonrpc":"2.0","method":"opaque/notification","extra":[1,2]}]).to_string()).into_response(),
    }
}
fn command(address: std::net::SocketAddr, temp: &std::path::Path, kind: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_t3-server"));
    command
        .arg(kind)
        .env("T3_ACP_MCP_ENDPOINT", format!("http://{address}/mcp"))
        .env("T3_ACP_MCP_AUTHORIZATION", "Bearer fixture-secret")
        .env("T3CODE_BOOTSTRAP_FD", "987654")
        .env("T3CODE_HOME", temp)
        .current_dir(temp)
        .kill_on_drop(true);
    command
}
#[tokio::test]
async fn native_stdio_preserves_opaque_requests_orders_handshake_and_bypasses_it_for_responses() {
    tokio::time::timeout(std::time::Duration::from_secs(15),async{
    let temp=tempfile::tempdir().unwrap();let peer=Peer::default();let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let address=listener.local_addr().unwrap();let router=axum::Router::new().route("/mcp",axum::routing::post(handle)).with_state(peer.clone());let mut server=tokio::task::JoinSet::new();server.spawn(async move{axum::serve(listener,router).await.unwrap();});
    let mut child=command(address,temp.path(),"acp-mcp-bridge").stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();let mut input=child.stdin.take().unwrap();let mut output=BufReader::new(child.stdout.take().unwrap()).lines();
    let raw="{ \"jsonrpc\": \"2.0\", \"id\": 1, \"method\": \"initialize\", \"foreign\": [1,{\"foo\":true}] }";
    input.write_all(format!("{raw}\r\n{{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}}\n{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"opaque/call\",\"extra\":true}}\n").as_bytes()).await.unwrap();
    let reverse:Value=serde_json::from_str(&output.next_line().await.unwrap().unwrap()).unwrap();assert_eq!(reverse["method"],"opaque/request");assert_eq!(reverse["unknown"],true);
    assert_eq!(peer.records.lock().unwrap().len(),1);
    input.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":\"reverse\",\"result\":{\"opaque\":true}}\n").await.unwrap();
    let initialized:Value=serde_json::from_str(&output.next_line().await.unwrap().unwrap()).unwrap();assert_eq!(initialized["result"]["text"],"👩‍💻中文");assert_eq!(initialized["unknown"]["kept"],true);
    let response:Value=serde_json::from_str(&output.next_line().await.unwrap().unwrap()).unwrap();assert_eq!(response["id"],2);assert_eq!(response["result"]["unknown"]["preserved"],true);
    let notification:Value=serde_json::from_str(&output.next_line().await.unwrap().unwrap()).unwrap();assert_eq!(notification["extra"],json!([1,2]));
    input.write_all(b"{\"id\":3,\"method\":\"quiet\"}\n{\"id\":4,\"method\":\"fail\"}\n{\"method\":\"fail\"}\nnot json\nnull\n7\n").await.unwrap();drop(input);
    let mut results=Vec::new();while let Some(line)=output.next_line().await.unwrap(){results.push(serde_json::from_str::<Value>(&line).unwrap());}
    assert_eq!(results.len(),2);assert!(results.iter().any(|value|value["error"]["code"]==-32700));assert!(results.iter().any(|value|value["id"]==4&&value["error"]["message"]=="T3 Code MCP endpoint responded with HTTP 403."));
    assert!(child.wait().await.unwrap().success());
    let records=peer.records.lock().unwrap();assert_eq!(records[0].1,raw);assert_eq!(records[1].0["mcp-session-id"],"stdio-session");
    for (headers,raw) in records.iter().skip(2){assert_eq!(headers["mcp-protocol-version"],"custom-version");assert_eq!(headers["authorization"],"Bearer fixture-secret");assert!(!raw.contains("fixture-secret"));}
    assert!(!temp.path().join("userdata").exists());server.abort_all();while server.join_next().await.is_some(){}
 }).await.expect("native bridge fixture timed out");
}
#[tokio::test]
async fn native_call_and_fast_path_validation_keep_credentials_out_of_argv_and_diagnostics() {
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        let temp = tempfile::tempdir().unwrap();
        let peer = Peer::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = axum::Router::new()
            .route("/mcp", axum::routing::post(handle))
            .with_state(peer.clone());
        let mut server = tokio::task::JoinSet::new();
        server.spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let result = command(address, temp.path(), "acp-mcp-call")
            .args(["foreign_tool", "{\"opaque\":\"中文\"}"])
            .output()
            .await
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&result.stdout).unwrap(),
            json!({"opaque":["tool",{"opaque":"中文"}]})
        );
        let records = peer.records.lock().unwrap();
        assert_eq!(records.len(), 3);
        for (headers, _) in records.iter().skip(1) {
            assert_eq!(headers["mcp-session-id"], "call-session");
            assert_eq!(headers["mcp-protocol-version"], "custom-version");
        }
        drop(records);
        let result = command(address, temp.path(), "acp-mcp-call")
            .env("T3_ACP_MCP_AUTHORIZATION", "Bearer array")
            .args(["x", "{}"])
            .output()
            .await
            .unwrap();
        assert_eq!(result.status.code(), Some(1));
        assert_eq!(
            String::from_utf8(result.stderr).unwrap(),
            "T3 Code MCP endpoint rejected initialization.\n"
        );
        for (args, message) in [
            (vec![], "acp-mcp-call requires <tool> and <arguments-json>."),
            (
                vec!["x", "invalid"],
                "acp-mcp-call arguments must be valid JSON.",
            ),
            (
                vec!["x", "null"],
                "acp-mcp-call arguments must be a JSON object.",
            ),
        ] {
            let result = command(address, temp.path(), "acp-mcp-call")
                .args(args)
                .output()
                .await
                .unwrap();
            assert_eq!(result.status.code(), Some(2));
            assert_eq!(
                String::from_utf8(result.stderr).unwrap(),
                format!("{message}\n")
            );
        }
        let result = command(address, temp.path(), "acp-mcp-bridge")
            .env_remove("T3_ACP_MCP_AUTHORIZATION")
            .output()
            .await
            .unwrap();
        assert_eq!(result.status.code(), Some(2));
        assert_eq!(
            String::from_utf8(result.stderr).unwrap(),
            "acp-mcp-bridge requires T3_ACP_MCP_ENDPOINT and T3_ACP_MCP_AUTHORIZATION.\n"
        );
        assert!(!temp.path().join("userdata").exists());
        server.abort_all();
        while server.join_next().await.is_some() {}
    })
    .await
    .expect("native MCP call fixture timed out");
}

#[tokio::test]
async fn native_bridge_keeps_normal_calls_concurrent_and_cancels_held_response_on_owned_exit() {
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        let temp = tempfile::tempdir().unwrap();
        let peer = Peer::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = axum::Router::new()
            .route("/mcp", axum::routing::post(handle))
            .with_state(peer.clone());
        let mut server = tokio::task::JoinSet::new();
        server.spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let mut child = command(address, temp.path(), "acp-mcp-bridge")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap()).lines();
        input
            .write_all(b"{\"id\":10,\"method\":\"held\"}\n")
            .await
            .unwrap();
        peer.held.notified().await;
        input
            .write_all(b"{\"id\":11,\"method\":\"opaque/call\"}\n")
            .await
            .unwrap();
        let result: Value =
            serde_json::from_str(&output.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(result["id"], 11);
        assert_eq!(result["result"]["unknown"]["preserved"], true);
        child.start_kill().unwrap();
        child.wait().await.unwrap();
        drop(input);
        peer.dropped.notified().await;
        server.abort_all();
        while server.join_next().await.is_some() {}
    })
    .await
    .expect("owned bridge cancellation fixture timed out");
}
