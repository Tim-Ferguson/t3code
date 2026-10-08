//! Authenticated allowlisted media proxy; the vendor shell surface is excluded.
use crate::transport::ApiState;
use axum::{
    body::Body,
    extract::{
        Request, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use t3_contracts::{AuthEnvironmentScope, DeviceHostId};

pub fn route_scope(
    path: &str,
    method: &Method,
    upgrade: bool,
) -> Result<AuthEnvironmentScope, StatusCode> {
    let matches = |pattern: &str| regex::Regex::new(pattern).unwrap().is_match(path);
    if upgrade {
        if !matches(r"^(?:/api/devices/ws|/vendor/serve-sim/helper/ws|/vendor/serve-emu/ws)$") {
            return Err(StatusCode::NOT_FOUND);
        }
        return Ok(if path == "/api/devices/ws" {
            AuthEnvironmentScope::OrchestrationRead
        } else {
            AuthEnvironmentScope::OrchestrationOperate
        });
    }
    if !matches(
        r"^(?:/api/devices|/vendor/serve-sim/api(?:/screenshot|/event-log(?:/events)?)?|/vendor/serve-sim/helper/[^/]+/(?:stream\.mjpeg|stream\.avcc|config|health|ax|foreground)|/vendor/serve-sim/helper/[^/]+/panel/(?:1|3)/stream\.avcc|/vendor/serve-sim/appstate|/vendor/serve-emu/api/(?:devices|screenshot|stream-mode|stream-settings|accessibility|fold)|/vendor/serve-emu/health)$",
    ) {
        return Err(StatusCode::NOT_FOUND);
    }
    let read = matches!(*method, Method::GET | Method::HEAD);
    if !read
        && !matches(
            r"^/vendor/(?:serve-sim/api/screenshot|serve-emu/api/(?:screenshot|stream-mode|stream-settings|fold))$",
        )
    {
        return Err(StatusCode::METHOD_NOT_ALLOWED);
    }
    Ok(
        if !read && matches(r"/api/(?:stream-mode|stream-settings|fold)$") {
            AuthEnvironmentScope::OrchestrationOperate
        } else {
            AuthEnvironmentScope::OrchestrationRead
        },
    )
}
fn request_headers(headers: &HeaderMap, origin: &str) -> HeaderMap {
    let mut result = headers.clone();
    for name in [
        "host",
        "connection",
        "upgrade",
        "sec-websocket-key",
        "sec-websocket-version",
        "sec-websocket-extensions",
        "sec-websocket-protocol",
        "cookie",
        "authorization",
        "dpop",
        "content-length",
        "accept-encoding",
    ] {
        result.remove(name);
    }
    if result.contains_key(header::ORIGIN) {
        result.insert(
            header::ORIGIN,
            origin.parse().expect("owned loopback origin"),
        );
    }
    result
}
pub async fn handle(
    State(state): State<ApiState>,
    ws: Result<WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>,
    request: Request,
) -> Response {
    let Ok(url) = url::Url::parse(&format!("http://localhost{}", request.uri())) else {
        return (StatusCode::BAD_REQUEST, "Bad Request").into_response();
    };
    let path = url
        .path()
        .strip_prefix(crate::device_service::DEVICE_HUB_ROUTE_PREFIX)
        .filter(|path| !path.is_empty())
        .unwrap_or("/");
    let upgrade = request
        .headers()
        .get(header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
    let scope = match route_scope(path, request.method(), upgrade) {
        Ok(scope) => scope,
        Err(code) => return code.into_response(),
    };
    let query = url.query_pairs().collect::<Vec<_>>();
    let ticket = query
        .iter()
        .find(|(key, _)| key == "wsTicket")
        .map(|(_, value)| value.as_ref());
    if let Err(error) =
        crate::transport::authenticate_media_request(&state, request.headers(), ticket, scope)
    {
        return error;
    }
    let Some(service) = state.devices.clone() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "Device hub is not running").into_response();
    };
    let host = query
        .iter()
        .find(|(key, _)| key == "hostId")
        .map(|(_, value)| value.as_ref());
    let host = match host {
        None => None,
        Some(value) => match DeviceHostId::new(value) {
            Ok(id) if id.as_str() == value => Some(id),
            _ => {
                return (StatusCode::SERVICE_UNAVAILABLE, "Device hub is not running")
                    .into_response();
            }
        },
    };
    let Some(ready) = service.current_readiness(host.as_ref()) else {
        return (StatusCode::SERVICE_UNAVAILABLE, "Device hub is not running").into_response();
    };
    let query = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(
            query
                .iter()
                .filter(|(key, _)| key != "wsTicket" && key != "hostId")
                .map(|(key, value)| (key.as_ref(), value.as_ref())),
        )
        .finish();
    let upstream = format!(
        "{}{path}{}",
        ready.origin,
        if query.is_empty() {
            String::new()
        } else {
            format!("?{query}")
        }
    );
    if upgrade {
        let Ok(ws) = ws else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        return ws
            .max_message_size(100 * 1024 * 1024)
            .max_frame_size(100 * 1024 * 1024)
            .on_upgrade(move |socket| {
                proxy_socket(socket, upstream.replacen("http", "ws", 1), service)
            });
    }
    let (parts, body) = request.into_parts();
    let client = match reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            tracing::error!(%error,"could not create device media client");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let mut upstream_request = client
        .request(parts.method.clone(), upstream)
        .headers(request_headers(&parts.headers, &ready.origin))
        // Original Fetch adds this after stripping the client's header on HTTP loopback.
        .header(header::ACCEPT_ENCODING, "gzip, deflate");
    if !matches!(parts.method, Method::GET | Method::HEAD) {
        upstream_request =
            upstream_request.body(reqwest::Body::wrap_stream(body.into_data_stream()));
    }
    let response = tokio::select! {biased;_=service.closed()=>return StatusCode::SERVICE_UNAVAILABLE.into_response(),response=upstream_request.send()=>match response{Ok(response)=>response,Err(error)=>{tracing::debug!(%error,"device media upstream unavailable");return StatusCode::INTERNAL_SERVER_ERROR.into_response();}}};
    let status = response.status();
    let mut headers = response.headers().clone();
    for name in ["content-encoding", "transfer-encoding", "connection"] {
        headers.remove(name);
    }
    headers.insert(
        header::CACHE_CONTROL,
        "no-store, no-transform".parse().unwrap(),
    );
    let stream = response.bytes_stream().take_until(async move {
        service.closed().await;
    });
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}
const MAX_SOCKET_PAYLOAD: usize = 100 * 1024 * 1024;
fn upstream_socket_config() -> tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
    // Node ws defaults to 100 MiB for the source proxy client.
    tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(MAX_SOCKET_PAYLOAD))
        .max_frame_size(Some(MAX_SOCKET_PAYLOAD))
}
async fn proxy_socket(
    client: WebSocket,
    upstream: String,
    service: crate::device_service::DeviceService,
) {
    let upstream = tokio::select! {biased;_=service.closed()=>return,result=tokio::time::timeout(Duration::from_secs(10),tokio_tungstenite::connect_async_with_config(upstream, Some(upstream_socket_config()), false))=>match result{Ok(Ok((socket,_)))=>socket,_=>return}};
    let (mut client_write, mut client_read) = client.split();
    let (mut upstream_write, mut upstream_read) = upstream.split();
    let forward = async {
        while let Some(Ok(frame)) = client_read.next().await {
            use tokio_tungstenite::tungstenite::Message as Target;
            let frame = match frame {
                Message::Text(text) => Target::Text(text.to_string().into()),
                Message::Binary(bytes) => Target::Binary(bytes),
                Message::Ping(_) | Message::Pong(_) => continue,
                Message::Close(frame) => Target::Close(frame.map(|frame| {
                    tokio_tungstenite::tungstenite::protocol::CloseFrame {
                        code: frame.code.into(),
                        reason: frame.reason.to_string().into(),
                    }
                })),
            };
            if upstream_write.send(frame).await.is_err() {
                break;
            }
        }
    };
    let reverse = async {
        while let Some(Ok(frame)) = upstream_read.next().await {
            use tokio_tungstenite::tungstenite::Message as Source;
            let frame = match frame {
                Source::Text(text) => Message::Text(text.to_string().into()),
                Source::Binary(bytes) => Message::Binary(bytes),
                Source::Ping(_) | Source::Pong(_) | Source::Frame(_) => continue,
                Source::Close(frame) => {
                    Message::Close(frame.map(|frame| axum::extract::ws::CloseFrame {
                        code: frame.code.into(),
                        reason: frame.reason.to_string().into(),
                    }))
                }
            };
            if client_write.send(frame).await.is_err() {
                break;
            }
        }
    };
    tokio::select! {_=service.closed()=>{},_=forward=>{},_=reverse=>{}}
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    #[test]
    fn original_proxy_routes_methods_scope_and_credential_header_policy_match() {
        let mut failures = Vec::new();
        let mut count = 0;
        for (index, line) in include_str!("../tests/fixtures/device-proxy.jsonl")
            .lines()
            .enumerate()
        {
            let row: Value = serde_json::from_str(line).unwrap();
            if row["headers"] == true {
                let mut headers = HeaderMap::new();
                for (key, value) in [
                    ("authorization", "Bearer private"),
                    ("cookie", "private=secret"),
                    ("dpop", "private"),
                    ("host", "t3.test"),
                    ("origin", "http://t3.test"),
                    ("x-fixture", "preserved"),
                    ("accept-encoding", "gzip"),
                ] {
                    headers.insert(
                        axum::http::HeaderName::from_bytes(key.as_bytes()).unwrap(),
                        value.parse().unwrap(),
                    );
                }
                let result = request_headers(&headers, "http://hub.test");
                assert_eq!(result["origin"], "http://hub.test");
                assert_eq!(result["x-fixture"], "preserved");
                for key in ["authorization", "cookie", "dpop", "host", "accept-encoding"] {
                    assert!(!result.contains_key(key));
                }
            } else {
                let actual = match route_scope(
                    row["path"].as_str().unwrap(),
                    &row["method"].as_str().unwrap().parse().unwrap(),
                    row["upgrade"].as_bool().unwrap(),
                ) {
                    Ok(scope) => json!({"scope":scope,"status":null}),
                    Err(status) => json!({"scope":null,"status":status.as_u16()}),
                };
                if actual["scope"] != row["scope"] || actual["status"] != row["status"] {
                    failures.push(format!("{index} actual={actual} source={row}"));
                }
            }
            count += 1;
        }
        assert_eq!(count, 331);
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }
}

#[cfg(all(test, unix))]
mod integration_tests {
    use super::*;
    use crate::{auth::AuthService, persistence::Store, transport::router};
    use serde_json::{Value, json};
    async fn milestone(socket: &tokio::net::UnixDatagram, path: &str) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let mut bytes = [0; 8192];
                let length = socket.recv(&mut bytes).await.unwrap();
                let event: Value = serde_json::from_slice(&bytes[..length]).unwrap();
                if event["path"] == path {
                    return;
                }
            }
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn owned_http_and_binary_websocket_proxy_strip_credentials_stream_and_close_upstream() {
        tokio::time::timeout(Duration::from_secs(20),async {
            let root=tempfile::tempdir().unwrap();let fixture=crate::device_service::tests::fixture(root.path()).await;
            let store=Store::memory().unwrap();let auth=AuthService::new(store.clone(),[75;32],"proxy_fixture".into(),"loopback-browser".into()).unwrap();
            let (reader,token)=auth.issue_session("fixture","bearer-access-token",vec![AuthEnvironmentScope::OrchestrationRead],json!({"deviceType":"unknown"}),chrono::Utc::now(),chrono::Duration::hours(1)).unwrap();
            let state=ApiState{store,auth:auth.clone(),environment:json!({}),config:None,settings:Some(fixture.settings.clone()),cors_origins:None,assets:None,providers:None,execution:None,workspace:None,terminals:None,discovery:None,resource_telemetry:None,host_resources:None,background:None,device_hosts:None,devices:Some(fixture.service.clone()),provider_auth:None};
            let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let address=listener.local_addr().unwrap();let (stop,stopped)=tokio::sync::oneshot::channel();let mut server=tokio::task::JoinSet::new();
            server.spawn(async move {axum::serve(listener,router(state)).with_graceful_shutdown(async {let _=stopped.await;}).await.unwrap();});
            let origin=format!("http://{address}/api/device-hub");let client=reqwest::Client::new();
            let inactive=client.get(format!("{origin}/api/devices")).bearer_auth(&token).send().await.unwrap();assert_eq!(inactive.status(),StatusCode::SERVICE_UNAVAILABLE);assert!(fixture.service.current_readiness(None).is_none());
            fixture.service.configure(serde_json::from_value(json!({"enabled":true})).unwrap()).await.unwrap();fixture.service.list().await.unwrap();
            let ticket=auth.issue_websocket_ticket(&reader,chrono::Utc::now()).unwrap()["ticket"].as_str().unwrap().to_owned();
            let health=client.get(format!("{origin}/vendor/serve-sim/helper/proxy/health?wsTicket={ticket}&hostId=local&device=a%2Fb&q=a+b")).bearer_auth(&token).header("cookie","private=fixture").header("dpop","fixture").header("origin","http://t3.test").header("x-fixture","preserved").header("accept-encoding","gzip").send().await.unwrap();
            assert_eq!(health.status(),StatusCode::OK);assert_eq!(health.headers()[header::CACHE_CONTROL],"no-store, no-transform");let health:Value=health.json().await.unwrap();assert_eq!(health["path"],"/vendor/serve-sim/helper/proxy/health?device=a%2Fb&q=a+b");
            let headers=health["headers"].as_object().unwrap();for forbidden in ["authorization","cookie","dpop"] {assert!(!headers.keys().any(|name|name.eq_ignore_ascii_case(forbidden)));}
            assert_eq!(headers.iter().find(|(key,_)|key.eq_ignore_ascii_case("origin")).unwrap().1,&json!(fixture.service.current_readiness(None).unwrap().origin));
            assert_eq!(headers.iter().find(|(key,_)|key.eq_ignore_ascii_case("x-fixture")).unwrap().1,"preserved");
            assert_eq!(headers.iter().find(|(key,_)|key.eq_ignore_ascii_case("accept-encoding")).unwrap().1,"gzip, deflate");
            use base64::Engine;
            for row in include_str!("../tests/fixtures/device-proxy-compression.jsonl").lines() {
                let row:Value=serde_json::from_str(row).unwrap();
                let response=client.get(format!("{origin}/vendor/serve-sim/helper/proxy/health?encoding={}",row["encoding"].as_str().unwrap())).bearer_auth(&token).header("accept-encoding","fixture-client-value").send().await.unwrap();
                assert_eq!(response.status().as_u16(),row["status"].as_u64().unwrap() as u16);assert!(!response.headers().contains_key(header::CONTENT_ENCODING));
                assert_eq!(response.headers()[header::CACHE_CONTROL],row["cacheControl"].as_str().unwrap());
                assert_eq!(response.bytes().await.unwrap().as_ref(),base64::engine::general_purpose::STANDARD.decode(row["body"].as_str().unwrap()).unwrap());
            }
            assert_eq!(client.get(format!("{origin}/api/devices?wsTicket={ticket}")).send().await.unwrap().status(),StatusCode::OK);
            assert_eq!(client.get(format!("{origin}/api/devices?wsTicket=invalid")).send().await.unwrap().status(),StatusCode::UNAUTHORIZED);
            assert_eq!(client.post(format!("{origin}/vendor/serve-sim/exec")).send().await.unwrap().status(),StatusCode::NOT_FOUND);
            assert_eq!(client.post(format!("{origin}/api/devices")).bearer_auth(&token).send().await.unwrap().status(),StatusCode::METHOD_NOT_ALLOWED);
            assert_eq!(client.post(format!("{origin}/vendor/serve-emu/api/stream-settings")).bearer_auth(&token).send().await.unwrap().status(),StatusCode::FORBIDDEN);
            let (_,operator)=auth.issue_session("operator","bearer-access-token",vec![AuthEnvironmentScope::OrchestrationOperate],json!({"deviceType":"unknown"}),chrono::Utc::now(),chrono::Duration::hours(1)).unwrap();
            let changed=client.post(format!("{origin}/vendor/serve-emu/api/stream-settings")).bearer_auth(&operator).header("origin","http://t3.test").json(&json!({"device":"fixture-android","quality":"fixture 👋"})).send().await.unwrap();assert_eq!(changed.status(),StatusCode::OK);let changed:Value=changed.json().await.unwrap();assert_eq!(changed["body"],json!({"device":"fixture-android","quality":"fixture 👋"}));
            let screenshot=client.post(format!("{origin}/vendor/serve-sim/api/screenshot?device=fixture-ios")).bearer_auth(&token).send().await.unwrap();assert_eq!(screenshot.status(),StatusCode::OK);assert_eq!(screenshot.bytes().await.unwrap().as_ref(),b"\x89PNG\r\n\x1a\nfixture screenshot");
            let mut stream=client.get(format!("{origin}/vendor/serve-sim/helper/proxy/stream.avcc")).bearer_auth(&token).send().await.unwrap();assert_eq!(stream.chunk().await.unwrap().unwrap().as_ref(),b"\x00stream-frame\xff");drop(stream);milestone(&fixture.socket,"/vendor/serve-sim/helper/proxy/stream.avcc#closed").await;
            let denied=client.get(format!("{origin}/vendor/serve-emu/ws")).bearer_auth(&token).header("upgrade","websocket").send().await.unwrap();assert_eq!(denied.status(),StatusCode::FORBIDDEN);
            let ticket=auth.issue_websocket_ticket(&reader,chrono::Utc::now()).unwrap()["ticket"].as_str().unwrap().to_owned();
            let (mut socket,_)=tokio_tungstenite::connect_async(format!("ws://{address}/api/device-hub/api/devices/ws?wsTicket={ticket}&hostId=local")).await.unwrap();
            use tokio_tungstenite::tungstenite::Message as Frame;
            assert_eq!(socket.next().await.unwrap().unwrap(),Frame::Binary(b"\x00fixture\xff".to_vec().into()));let payload=b"\x00typed-input\xff";socket.send(Frame::Binary(payload.to_vec().into())).await.unwrap();assert_eq!(socket.next().await.unwrap().unwrap(),Frame::Binary(payload.to_vec().into()));socket.close(None).await.unwrap();drop(socket);milestone(&fixture.socket,"/api/devices/ws#closed").await;
            let ticket=auth.issue_websocket_ticket(&reader,chrono::Utc::now()).unwrap()["ticket"].as_str().unwrap().to_owned();
            let (mut large_socket,_)=tokio_tungstenite::connect_async_with_config(format!("ws://{address}/api/device-hub/api/devices/ws?wsTicket={ticket}&large=1"),Some(upstream_socket_config()),false).await.unwrap();
            let frame=large_socket.next().await.unwrap().unwrap().into_data();assert_eq!(frame.len(),65*1024*1024);assert!(frame.iter().all(|byte|*byte==123));drop(frame);large_socket.close(None).await.unwrap();drop(large_socket);milestone(&fixture.socket,"/api/devices/ws?large=1#closed").await;
            let mut pending_stream=client.get(format!("{origin}/vendor/serve-sim/helper/proxy/stream.avcc")).bearer_auth(&token).send().await.unwrap();assert!(pending_stream.chunk().await.unwrap().is_some());
            let ticket=auth.issue_websocket_ticket(&reader,chrono::Utc::now()).unwrap()["ticket"].as_str().unwrap().to_owned();
            let (mut pending_socket,_)=tokio_tungstenite::connect_async(format!("ws://{address}/api/device-hub/api/devices/ws?wsTicket={ticket}")).await.unwrap();assert!(pending_socket.next().await.unwrap().unwrap().is_binary());
            fixture.service.shutdown().await;
            assert!(pending_stream.chunk().await.unwrap().is_none());
            assert!(matches!(pending_socket.next().await,None|Some(Err(_))|Some(Ok(Frame::Close(_)))));
fixture.settings.shutdown().await;stop.send(()).unwrap();server.join_next().await.unwrap().unwrap();
        }).await.unwrap();
    }
}
