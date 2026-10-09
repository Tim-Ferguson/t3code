//! Serving `github-media` assets: the credential goes only to GitHub's own hosts, a redirect leads
//! to a signed object that never sees it, and only pictures and recordings leave. The requests go
//! to a loopback server; the host policy still decides on the real GitHub URLs.
use axum::{
    Router,
    body::Body,
    extract::State,
    http::{HeaderMap, Response, StatusCode, Uri},
    routing::any,
};
use std::sync::{Arc, Mutex};
use t3_server::asset_github_media::{Request, client, response};

#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<(String, Option<String>, Option<String>)>>>);

async fn upstream(State(seen): State<Seen>, uri: Uri, headers: HeaderMap) -> Response<Body> {
    let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok()).map(str::to_owned);
    seen.0.lock().unwrap().push((uri.path().to_owned(), header("authorization"), header("range")));
    let builder = Response::builder();
    match uri.path() {
        "/github.com/user-attachments/assets/redirects" => builder.status(302).header("location", "https://objects.githubusercontent.com/signed/shot.png?sig=1").body(Body::empty()),
        "/objects.githubusercontent.com/signed/shot.png" => builder.header("content-type", "image/png").header("etag", "\"abc\"").body(Body::from("png-bytes")),
        "/raw.githubusercontent.com/o/r/main/clip.mp4" => builder.header("content-type", "application/octet-stream").body(Body::from("mp4-bytes")),
        "/raw.githubusercontent.com/o/r/main/page.html" => builder.header("content-type", "text/html").body(Body::from("<script>")),
        "/raw.githubusercontent.com/o/r/main/vector.svg" => builder.header("content-type", "image/svg+xml").body(Body::from("<svg/>")),
        "/raw.githubusercontent.com/o/r/main/missing.png" => builder.status(404).header("content-length", "9").body(Body::from("not found")),
        "/raw.githubusercontent.com/o/r/main/broken.png" => builder.status(500).body(Body::empty()),
        "/raw.githubusercontent.com/o/r/main/loop.png" => builder.status(302).header("location", "https://raw.githubusercontent.com/o/r/main/loop.png").body(Body::empty()),
        "/raw.githubusercontent.com/o/r/main/downgrade.png" => builder.status(302).header("location", "http://raw.githubusercontent.com/o/r/main/x.png").body(Body::empty()),
        _ => builder.status(StatusCode::NOT_FOUND).body(Body::empty()),
    }
    .unwrap()
}

#[tokio::test]
async fn the_credential_stays_on_github_and_only_media_is_served() {
    let seen = Seen::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().fallback(any(upstream)).with_state(seen.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    // https://host/path -> http://loopback/host/path
    let connect_to = move |target: &str| {
        let url = url::Url::parse(target).unwrap();
        format!("{origin}/{}{}", url.host_str().unwrap(), url.path())
    };
    let http = client();
    let get = |url: &'static str, range: Option<&'static str>| {
        let (http, connect_to) = (&http, &connect_to);
        async move {
            let request = Request { url, expires_at: 1_000_000 + 600_000, now_ms: 1_000_000, range, if_range: None, token: Some("gh-token"), connect_to };
            response(http, &request).await
        }
    };
    let body = |response: axum::response::Response| async move { axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap().to_vec() };

    // GitHub redirects an attachment to a signed object: the token goes to GitHub, not to the object.
    let reply = get("https://github.com/user-attachments/assets/redirects", Some("bytes=0-3")).await;
    assert_eq!(reply.status(), 200);
    assert_eq!(reply.headers()["content-type"], "image/png");
    assert_eq!(reply.headers()["etag"], "\"abc\"");
    assert_eq!(reply.headers()["cache-control"], "private, max-age=600");
    assert_eq!(reply.headers()["x-content-type-options"], "nosniff");
    assert_eq!(body(reply).await, b"png-bytes");
    let requests = seen.0.lock().unwrap().clone();
    assert_eq!(requests[0], ("/github.com/user-attachments/assets/redirects".into(), Some("Bearer gh-token".into()), Some("bytes=0-3".into())));
    assert_eq!(requests[1], ("/objects.githubusercontent.com/signed/shot.png".into(), None, Some("bytes=0-3".into())));

    // The raw host labels every binary octet-stream, so a recording is typed by its name.
    let video = get("https://raw.githubusercontent.com/o/r/main/clip.mp4", None).await;
    assert_eq!((video.status().as_u16(), video.headers()["content-type"].to_str().unwrap()), (200, "video/mp4"));
    // A document is never served from this origin, and an SVG carries the sandbox.
    assert_eq!(get("https://raw.githubusercontent.com/o/r/main/page.html", None).await.status(), 415);
    let svg = get("https://raw.githubusercontent.com/o/r/main/vector.svg", None).await;
    assert_eq!(svg.headers()["content-security-policy"], "default-src 'none'; style-src 'unsafe-inline'; sandbox");
    // An upstream refusal is the client's answer without the upstream's entity headers; a broken hop is 502.
    let missing = get("https://raw.githubusercontent.com/o/r/main/missing.png", None).await;
    assert_eq!(missing.status(), 404);
    assert!(missing.headers().get("content-length").is_none());
    assert_eq!(get("https://raw.githubusercontent.com/o/r/main/broken.png", None).await.status(), 502);
    // A redirect loop and a downgrade to http are not GitHub answering with bytes.
    assert_eq!(get("https://raw.githubusercontent.com/o/r/main/loop.png", None).await.status(), 502);
    assert_eq!(get("https://raw.githubusercontent.com/o/r/main/downgrade.png", None).await.status(), 502);
    // Past the signed URL's expiry nothing may be cached.
    let request = Request { url: "https://raw.githubusercontent.com/o/r/main/clip.mp4", expires_at: 5, now_ms: 1_000_000, range: None, if_range: None, token: None, connect_to: &connect_to };
    assert_eq!(response(&http, &request).await.headers()["cache-control"], "private, no-store");
    server.abort();
}
