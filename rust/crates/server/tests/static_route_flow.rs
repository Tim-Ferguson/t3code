//! The web app route against the policy of the original's static handler (`handleStaticAndDevRequest`): the bundle's files with
//! validators and conditional answers, the cache policy of the build manifest, the SPA fallback, the refusals of a path that leaves
//! the bundle, and the dev URL redirect. The router is the server's own, served from a temporary bundle.
use axum::{
    body::Body,
    http::{Method, Request, StatusCode, header},
};
use serde_json::json;
use std::path::Path;
use t3_server::{
    auth::AuthService,
    persistence::Store,
    static_route::{self, StaticConfig},
    transport::{ApiState, router},
};
use tower::ServiceExt;

fn make_state(base: &Path, assets: Option<&Path>, dev_url: Option<&str>) -> ApiState {
    let store = Store::open(base.join("state.sqlite")).unwrap();
    let auth = AuthService::new(store.clone(), [3; 32], "t3_session_static".into(), "loopback-browser".into()).unwrap();
    static_route::install(&store, StaticConfig::with_dev_url(dev_url.map(|url| url.parse().unwrap())));
    ApiState {
        store,
        auth,
        environment: json!({}),
        config: None,
        settings: None,
        cors_origins: None,
        assets: assets.map(Path::to_path_buf),
        providers: None,
        execution: None,
        workspace: None,
        terminals: None,
        discovery: None,
        resource_telemetry: None,
        host_resources: None,
        background: None,
        device_hosts: None,
        devices: None,
        provider_auth: None,
    }
}

struct Answer {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Vec<u8>,
}

async fn ask(state: &ApiState, method: Method, uri: &str, headers: &[(&str, &str)]) -> Answer {
    let mut request = Request::builder().method(method).uri(uri).header(header::HOST, "box.example:3773");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = router(state.clone()).oneshot(request.body(Body::empty()).unwrap()).await.unwrap();
    let (parts, body) = response.into_parts();
    Answer { status: parts.status, headers: parts.headers, body: axum::body::to_bytes(body, usize::MAX).await.unwrap().to_vec() }
}

fn header_of<'a>(answer: &'a Answer, name: &str) -> Option<&'a str> {
    answer.headers.get(name).and_then(|value| value.to_str().ok())
}

fn bundle(base: &Path) -> std::path::PathBuf {
    let root = base.join("client");
    std::fs::create_dir_all(root.join("assets")).unwrap();
    std::fs::create_dir_all(root.join(".vite")).unwrap();
    std::fs::create_dir_all(root.join("docs")).unwrap();
    std::fs::write(root.join("index.html"), "<html>app</html>").unwrap();
    std::fs::write(root.join("docs/index.html"), "<html>docs</html>").unwrap();
    std::fs::write(root.join("assets/index-abcd1234.js"), "export {}").unwrap();
    std::fs::write(root.join("assets/other-abcd1234.js"), "export {}").unwrap();
    std::fs::write(root.join("assets/app.wasm"), [0, 97, 115, 109]).unwrap();
    std::fs::write(root.join(".vite/manifest.json"), json!({"index.html": {"file": "assets/index-abcd1234.js", "css": ["assets/index-abcd1234.css"], "assets": ["assets/font-abcd1234.woff2"]}}).to_string()).unwrap();
    root
}

#[tokio::test]
async fn files_carry_validators_and_a_cache_policy() {
    let base = tempfile::Builder::new().prefix("t3code-rust-port-test-static-").tempdir().unwrap();
    let root = bundle(base.path());
    let state = make_state(base.path(), Some(&root), None);

    // HTML is never conditional and always revalidates.
    let index = ask(&state, Method::GET, "/", &[]).await;
    assert_eq!(index.status, StatusCode::OK);
    assert_eq!(index.body, b"<html>app</html>");
    assert_eq!(header_of(&index, "content-type"), Some("text/html; charset=utf-8"));
    assert_eq!(header_of(&index, "cache-control"), Some("no-cache"));
    assert!(index.headers.get("etag").is_none() && index.headers.get("last-modified").is_none());
    let revalidated = ask(&state, Method::GET, "/", &[("if-none-match", "*")]).await;
    assert_eq!(revalidated.status, StatusCode::OK);

    // A hashed asset the manifest names is immutable; one that is merely named alike is not.
    let named = ask(&state, Method::GET, "/assets/index-abcd1234.js", &[]).await;
    assert_eq!(header_of(&named, "cache-control"), Some("public, max-age=31536000, immutable"));
    assert_eq!(header_of(&named, "content-type"), Some("text/javascript"));
    let unnamed = ask(&state, Method::GET, "/assets/other-abcd1234.js", &[]).await;
    assert_eq!(header_of(&unnamed, "cache-control"), Some("no-cache"));
    let wasm = ask(&state, Method::GET, "/assets/app.wasm", &[]).await;
    assert_eq!((header_of(&wasm, "content-type"), wasm.body.clone()), (Some("application/wasm"), vec![0, 97, 115, 109]));

    // The weak validator is the size and the modification time in hex.
    let etag = header_of(&unnamed, "etag").unwrap().to_owned();
    let (size, modified) = etag.strip_prefix("W/\"").and_then(|rest| rest.strip_suffix('"')).and_then(|rest| rest.split_once('-')).unwrap();
    assert_eq!(u64::from_str_radix(size, 16).unwrap(), 9);
    let mtime = std::fs::metadata(root.join("assets/other-abcd1234.js")).unwrap().modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
    assert_eq!(u64::from_str_radix(modified, 16).unwrap(), mtime);
    let last_modified = header_of(&unnamed, "last-modified").unwrap().to_owned();
    assert!(last_modified.ends_with(" GMT"), "{last_modified}");

    // Conditional requests: If-None-Match wins over dates and compares weakly; the answer repeats the validators and varies on the encoding.
    for matching in [etag.as_str(), "*", &format!("\"nope\", {etag}"), &etag[2..]] {
        let answer = ask(&state, Method::GET, "/assets/other-abcd1234.js", &[("if-none-match", matching)]).await;
        assert_eq!(answer.status, StatusCode::NOT_MODIFIED, "{matching}");
        assert!(answer.body.is_empty());
        assert_eq!(header_of(&answer, "etag"), Some(etag.as_str()));
        assert_eq!(header_of(&answer, "vary"), Some("Accept-Encoding"));
        assert_eq!(header_of(&answer, "cache-control"), Some("no-cache"));
    }
    let other = ask(&state, Method::GET, "/assets/other-abcd1234.js", &[("if-none-match", "\"different\""), ("if-modified-since", &last_modified)]).await;
    assert_eq!(other.status, StatusCode::OK);
    let current = ask(&state, Method::GET, "/assets/other-abcd1234.js", &[("if-modified-since", &last_modified)]).await;
    assert_eq!(current.status, StatusCode::NOT_MODIFIED);
    let stale = ask(&state, Method::GET, "/assets/other-abcd1234.js", &[("if-modified-since", "Thu, 01 Jan 1970 00:00:01 GMT")]).await;
    assert_eq!(stale.status, StatusCode::OK);
    let nonsense = ask(&state, Method::GET, "/assets/other-abcd1234.js", &[("if-modified-since", "not a date")]).await;
    assert_eq!(nonsense.status, StatusCode::OK);

    // HEAD answers the headers.
    let head = ask(&state, Method::HEAD, "/assets/other-abcd1234.js", &[]).await;
    assert_eq!(head.status, StatusCode::OK);
    assert!(head.body.is_empty());
    assert_eq!(header_of(&head, "etag"), Some(etag.as_str()));
}

#[tokio::test]
async fn a_path_with_no_file_is_the_app_and_a_path_outside_the_bundle_is_refused() {
    let base = tempfile::Builder::new().prefix("t3code-rust-port-test-static-").tempdir().unwrap();
    let root = bundle(base.path());
    let state = make_state(base.path(), Some(&root), None);
    // A client-side route, a missing file, and a missing file with an extension all get the app.
    for uri in ["/threads/abc", "/missing.png", "/assets/nope.js"] {
        let answer = ask(&state, Method::GET, uri, &[]).await;
        assert_eq!((answer.status, answer.body.as_slice()), (StatusCode::OK, b"<html>app</html>".as_slice()), "{uri}");
    }
    // A directory is its own index.
    let docs = ask(&state, Method::GET, "/docs", &[]).await;
    assert_eq!(docs.body, b"<html>docs</html>");
    // The request path is not percent-decoded (a copy of the original): an escaped name is not found, and falls back to the app.
    std::fs::write(root.join("a b.txt"), "spaced").unwrap();
    assert_eq!(ask(&state, Method::GET, "/a%20b.txt", &[]).await.body, b"<html>app</html>");
    // Leaving the bundle is a 400, whatever the spelling that reaches the handler.
    for uri in ["/..", "/a/../../etc/passwd", "/../x"] {
        let answer = ask(&state, Method::GET, uri, &[]).await;
        assert_eq!((answer.status, answer.body.as_slice()), (StatusCode::BAD_REQUEST, b"Invalid static file path".as_slice()), "{uri}");
    }
    // Without a bundle there is nothing to serve.
    let bare = make_state(base.path(), None, None);
    let answer = ask(&bare, Method::GET, "/", &[]).await;
    assert_eq!((answer.status, answer.body.as_slice()), (StatusCode::SERVICE_UNAVAILABLE, b"No static directory configured and no dev URL set.".as_slice()));
}

#[tokio::test]
async fn a_server_with_a_dev_url_hands_loopback_requests_to_it() {
    let base = tempfile::Builder::new().prefix("t3code-rust-port-test-static-").tempdir().unwrap();
    let root = bundle(base.path());
    let state = make_state(base.path(), Some(&root), Some("http://127.0.0.1:5733/base?x=1"));
    // The page, asked for by a loopback name, is the dev server's; the path and query come along, the dev URL's own query does not.
    for host in ["localhost:3773", "127.0.0.1:3773", "[::1]:3773", "LOCALHOST"] {
        let request = Request::builder().uri("/threads/a?b=2").header(header::HOST, host).body(Body::empty()).unwrap();
        let response = router(state.clone()).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::FOUND, "{host}");
        assert_eq!(response.headers()[header::LOCATION], "http://127.0.0.1:5733/threads/a?b=2", "{host}");
    }
    // Another name is served the built app, and the paths the dev server proxies back are not the app.
    assert_eq!(ask(&state, Method::GET, "/threads/a", &[]).await.body, b"<html>app</html>");
    for uri in ["/api/nothing", "/oauth/x", "/.well-known/x", "/ws/x", "/mcp"] {
        let request = Request::builder().uri(uri).header(header::HOST, "localhost").body(Body::empty()).unwrap();
        let response = router(state.clone()).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
    }
    assert!(static_route::is_dev_proxied_path("/api") && !static_route::is_dev_proxied_path("/apis"));
}
