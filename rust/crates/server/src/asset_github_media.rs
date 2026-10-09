//! Serves media a pull request body points at on GitHub (port of `assets/GitHubMediaFetch.ts`).
//! A private repository answers an unauthenticated request for an upload with 404, so the server
//! fetches it with the `gh` credential and hands the bytes back. The credential travels only to
//! GitHub's own hosts: a redirect leads to a signed object URL that authorizes on its own, and the
//! store behind it has no business seeing a token. Only pictures and recordings leave this origin.
use axum::{
    body::Body,
    http::{HeaderName, HeaderValue, StatusCode},
    response::Response,
};
use regex::Regex;
use std::sync::LazyLock;

/// Exactly the hosts the credential is for.
const CREDENTIALED_HOSTS: [&str; 4] = ["github.com", "www.github.com", "raw.githubusercontent.com", "media.githubusercontent.com"];
const MAX_REDIRECTS: usize = 3;
/// Passed through so a seek in a long video costs one upstream range request, not a full download.
const FORWARDED_REQUEST_HEADERS: [&str; 2] = ["range", "if-range"];
const FORWARDED_RESPONSE_HEADERS: [&str; 6] = ["content-type", "content-length", "content-range", "accept-ranges", "etag", "last-modified"];
const SVG_CONTENT_SECURITY_POLICY: &str = "default-src 'none'; style-src 'unsafe-inline'; sandbox";

static MEDIA_CONTENT_TYPE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(?:image|video|audio)/[A-Za-z0-9_!#$&^.+-]+$").unwrap());

fn is_credentialed_host(url: &str) -> bool {
    url::Url::parse(url).ok().and_then(|url| url.host_str().map(|host| CREDENTIALED_HOSTS.contains(&host.to_lowercase().as_str()))).unwrap_or(false)
}

/// What the response needs from the caller's request.
pub struct Request<'a> {
    pub url: &'a str,
    pub expires_at: i64,
    pub now_ms: i64,
    pub range: Option<&'a str>,
    pub if_range: Option<&'a str>,
    pub token: Option<&'a str>,
    /// Maps the URL a request is made to, so a test can reach a loopback server while the host
    /// policy above still decides on the real GitHub URL. Production leaves it as the identity.
    pub connect_to: &'a (dyn Fn(&str) -> String + Sync),
}

/// A name's type from its extension, for the raw host that labels every binary octet-stream.
fn type_from_name(file_name: &str) -> String {
    let lower = file_name.to_lowercase();
    let extension = lower.rsplit_once('.').map(|(_, extension)| extension).unwrap_or("");
    match extension {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "svg" => "image/svg+xml",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        _ => "",
    }
    .into()
}

/// Follows GitHub's redirect to the signed object itself, and never carries the credential off GitHub.
async fn fetch_following_redirects(client: &reqwest::Client, request: &Request<'_>) -> Option<reqwest::Response> {
    let mut target = request.url.to_string();
    for hop in 0..=MAX_REDIRECTS {
        let mut builder = client.get((request.connect_to)(&target)).header("accept-encoding", "identity");
        for (name, value) in [("range", request.range), ("if-range", request.if_range)] {
            if FORWARDED_REQUEST_HEADERS.contains(&name) {
                if let Some(value) = value {
                    builder = builder.header(name, value);
                }
            }
        }
        // The credential rides only on a request to GitHub itself, decided from the target.
        if let (Some(token), true) = (request.token, is_credentialed_host(&target)) {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        let response = builder.send().await.ok()?;
        let status = response.status().as_u16();
        if !(300..400).contains(&status) {
            return Some(response);
        }
        // A chain this long is not GitHub answering with bytes, and its body is not the media.
        let location = response.headers().get("location")?.to_str().ok()?.to_string();
        if hop >= MAX_REDIRECTS {
            return None;
        }
        let next = url::Url::parse(&target).ok()?.join(&location).ok()?;
        if next.scheme() != "https" {
            return None;
        }
        target = next.to_string();
    }
    None
}

fn empty(status: u16, headers: &[(&str, String)]) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    for (name, value) in headers {
        if let (Ok(name), Ok(value)) = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(value)) {
            response.headers_mut().insert(name, value);
        }
    }
    response
}

/// The response for a `github-media` asset: the upstream bytes when they are a picture or a
/// recording, 415 when they are anything else, the upstream's refusal (or 502 for its own
/// failure) otherwise.
pub async fn response(client: &reqwest::Client, request: &Request<'_>) -> Response {
    let remaining = (request.expires_at - request.now_ms).div_euclid(1000);
    let mut headers = vec![
        ("cache-control", if remaining > 0 { format!("private, max-age={remaining}") } else { "private, no-store".into() }),
        ("x-content-type-options", "nosniff".to_string()),
    ];
    let Some(upstream) = fetch_following_redirects(client, request).await else { return empty(502, &headers) };
    let status = upstream.status().as_u16();
    // An upstream refusal is the client's answer, not this server's fault; only a broken hop is.
    // It carries none of the upstream entity headers.
    if status >= 400 {
        return empty(if status >= 500 { 502 } else { status }, &headers);
    }
    let upstream_type = upstream.headers().get("content-type").and_then(|value| value.to_str().ok()).map(|value| value.split(';').next().unwrap_or("").trim().to_lowercase()).unwrap_or_default();
    let content_type = if MEDIA_CONTENT_TYPE.is_match(&upstream_type) {
        upstream_type
    } else {
        type_from_name(&crate::asset_workspace::github_media_file_name(request.url))
    };
    if !MEDIA_CONTENT_TYPE.is_match(&content_type) {
        return empty(415, &headers);
    }
    for name in FORWARDED_RESPONSE_HEADERS {
        if let Some(value) = upstream.headers().get(name).and_then(|value| value.to_str().ok()) {
            headers.retain(|(existing, _)| *existing != name);
            headers.push((name, value.to_string()));
        }
    }
    headers.retain(|(name, _)| *name != "content-type");
    headers.push(("content-type", content_type.clone()));
    if content_type == "image/svg+xml" {
        headers.push(("content-security-policy", SVG_CONTENT_SECURITY_POLICY.into()));
    }
    let mut response = Response::new(Body::from_stream(upstream.bytes_stream()));
    *response.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
    for (name, value) in &headers {
        if let (Ok(name), Ok(value)) = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(value)) {
            response.headers_mut().insert(name, value);
        }
    }
    response
}

/// A client that never follows a redirect itself: following here is what keeps the token on GitHub.
pub fn client() -> reqwest::Client {
    reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().expect("a plain HTTP client builds")
}
