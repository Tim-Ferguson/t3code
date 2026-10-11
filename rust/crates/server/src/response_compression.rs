//! Response compression, as the original's router applies it to every response (`HttpMiddleware.compression()` in
//! `layerHttpCompression`, `http.ts`): brotli, gzip or deflate by the request's `Accept-Encoding` in that order of preference
//! (levels 4, 6 and 6), only for text-like content types, only for bodies of 1024 bytes or more, never for an empty body, a
//! range or not-modified answer, a response that names its own `Content-Encoding` (an `identity` one has it removed) or one that
//! says `no-transform`. A response that could have been compressed gets `Vary: Accept-Encoding`, and a strong `ETag` of one that
//! was becomes weak.
//!
//! Differences from the original, kept deliberately: the answers that carry a credential are never compressed (the original
//! compresses every response, its own documentation of the middleware warns of BREACH, and a bearer token next to text an
//! attacker can make the server repeat is what that attack reads): those of `/api/auth/`, `/oauth/`, `/api/connect/`,
//! `/api/t3-connect/` and `/mcp`, and any that sets a cookie. A body of unknown length (a stream) is sent as it is, because compressing it
//! here would hold it back until it ended, and a body above [`LARGEST`] is too. Both are rare on this server: the web app's
//! files and JSON replies have a length.
use axum::{
    body::Body,
    extract::Request,
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::Next,
    response::Response,
};
use std::io::Write;

/// The smallest body that is compressed (`minSize`).
const SMALLEST: u64 = 1024;
/// The largest body that is compressed here: it is held in memory while it is.
const LARGEST: u64 = 32 << 20;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Algorithm {
    Brotli,
    Gzip,
    Deflate,
}

impl Algorithm {
    fn name(self) -> &'static str {
        match self {
            Algorithm::Brotli => "br",
            Algorithm::Gzip => "gzip",
            Algorithm::Deflate => "deflate",
        }
    }
}

/// `defaultCompressible`.
fn compressible(content_type: &str) -> bool {
    let kind = content_type.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    kind.starts_with("text/")
        || matches!(kind.as_str(), "application/json" | "application/javascript" | "application/xml" | "image/svg+xml" | "application/wasm")
        || kind.ends_with("+json")
        || kind.ends_with("+xml")
}

/// `parseAcceptEncoding`: the weight of each coding the header names, or none for a header that is empty or malformed.
fn parse_accept_encoding(header: &str) -> Option<Vec<(String, f32)>> {
    let trimmed = header.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut accepted = Vec::new();
    for part in trimmed.split(',') {
        let member = part.trim().to_ascii_lowercase();
        let (coding, weight) = match member.split_once(';') {
            None => (member.as_str(), 1.0),
            Some((coding, parameter)) => {
                let value = parameter.trim().strip_prefix("q=")?;
                // `0`, `0.xxx`, `1`, `1.000`: at most three decimals, and not above one.
                let (whole, fraction) = value.split_once('.').map_or((value, ""), |(whole, fraction)| (whole, fraction));
                let valid = fraction.len() <= 3 && fraction.bytes().all(|byte| byte.is_ascii_digit()) && (whole == "0" || (whole == "1" && fraction.bytes().all(|byte| byte == b'0')));
                if !valid {
                    return None;
                }
                (coding.trim(), value.parse::<f32>().ok()?)
            }
        };
        if coding.is_empty() || !coding.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+.^_`|~-".contains(&byte)) {
            return None;
        }
        accepted.retain(|(known, _): &(String, f32)| known != coding);
        accepted.push((coding.to_owned(), weight));
    }
    Some(accepted)
}

/// `negotiate`: the first of the server's preference the request accepts with a weight above zero.
fn negotiate(header: Option<&str>) -> Option<Algorithm> {
    let accepted = parse_accept_encoding(header?)?;
    let weight = |name: &str| accepted.iter().find(|(coding, _)| coding == name).or_else(|| accepted.iter().find(|(coding, _)| coding == "*")).map(|(_, weight)| *weight);
    [Algorithm::Brotli, Algorithm::Gzip, Algorithm::Deflate].into_iter().find(|algorithm| weight(algorithm.name()).is_some_and(|weight| weight > 0.0))
}

fn compress(algorithm: Algorithm, bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    match algorithm {
        Algorithm::Brotli => {
            let mut out = Vec::with_capacity(bytes.len() / 3);
            {
                let mut encoder = brotli::CompressorWriter::new(&mut out, 4096, 4, 22);
                encoder.write_all(bytes)?;
            }
            Ok(out)
        }
        Algorithm::Gzip => {
            let mut encoder = flate2::write::GzEncoder::new(Vec::with_capacity(bytes.len() / 3), flate2::Compression::new(6));
            encoder.write_all(bytes)?;
            encoder.finish()
        }
        Algorithm::Deflate => {
            let mut encoder = flate2::write::ZlibEncoder::new(Vec::with_capacity(bytes.len() / 3), flate2::Compression::new(6));
            encoder.write_all(bytes)?;
            encoder.finish()
        }
    }
}

fn with_vary(headers: &mut HeaderMap) {
    let existing = headers.get(header::VARY).and_then(|value| value.to_str().ok()).map(str::to_owned);
    let vary = match existing {
        None => "Accept-Encoding".to_owned(),
        Some(vary) => {
            let members: Vec<String> = vary.split(',').map(|member| member.trim().to_ascii_lowercase()).collect();
            if members.iter().any(|member| member == "*" || member == "accept-encoding") { vary } else { format!("{vary}, Accept-Encoding") }
        }
    };
    if let Ok(value) = HeaderValue::from_str(&vary) {
        headers.insert(header::VARY, value);
    }
}

/// The layer: runs the request, then compresses its answer if the rules above allow.
pub async fn layer(request: Request, next: Next) -> Response {
    let secret_bearing = ["/api/auth/", "/oauth/", "/api/connect/", "/api/t3-connect/", "/mcp"].iter().any(|prefix| request.uri().path().starts_with(prefix));
    let accept = request.headers().get(header::ACCEPT_ENCODING).and_then(|value| value.to_str().ok()).map(str::to_owned);
    let head = request.method() == Method::HEAD;
    let response = next.run(request).await;
    if secret_bearing || response.headers().contains_key(header::SET_COOKIE) {
        return response;
    }
    transform(accept.as_deref(), head, response).await
}

async fn transform(accept: Option<&str>, head: bool, response: Response) -> Response {
    let status = response.status();
    if status.as_u16() < 200 || matches!(status, StatusCode::NO_CONTENT | StatusCode::PARTIAL_CONTENT | StatusCode::NOT_MODIFIED) {
        return response;
    }
    if let Some(encoding) = response.headers().get(header::CONTENT_ENCODING) {
        if encoding.to_str().is_ok_and(|encoding| encoding.trim().eq_ignore_ascii_case("identity")) {
            let mut response = response;
            response.headers_mut().remove(header::CONTENT_ENCODING);
            return response;
        }
        return response;
    }
    let length = {
        use axum::body::HttpBody as _;
        response.body().size_hint().exact()
    }
    .or_else(|| response.headers().get(header::CONTENT_LENGTH).and_then(|value| value.to_str().ok()).and_then(|value| value.parse().ok()));
    if length == Some(0) {
        return response;
    }
    if response.headers().get(header::CACHE_CONTROL).and_then(|value| value.to_str().ok()).is_some_and(|value| value.split([',', ';']).any(|directive| directive.trim().eq_ignore_ascii_case("no-transform"))) {
        return response;
    }
    let compressible_type = response.headers().get(header::CONTENT_TYPE).and_then(|value| value.to_str().ok()).is_some_and(compressible);
    if !compressible_type {
        return response;
    }
    let mut response = response;
    let Some(algorithm) = negotiate(accept) else {
        with_vary(response.headers_mut());
        return response;
    };
    // A body of unknown length would have to be held to its end to be compressed, and one too large to hold is left alone.
    let Some(length) = length.filter(|length| *length <= LARGEST) else { return response };
    if length < SMALLEST {
        with_vary(response.headers_mut());
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let compressed = if head {
        // A HEAD answer states what the GET would: its headers are selected without reading the body.
        Vec::new()
    } else {
        let Ok(bytes) = axum::body::to_bytes(body, LARGEST as usize).await else {
            return Response::from_parts(parts, Body::empty());
        };
        match compress(algorithm, &bytes) {
            Ok(compressed) => compressed,
            Err(_) => return Response::from_parts(parts, Body::from(bytes)),
        }
    };
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static(algorithm.name()));
    with_vary(&mut parts.headers);
    let weak = parts.headers.get(header::ETAG).and_then(|value| value.to_str().ok()).filter(|etag| !etag.starts_with("W/")).map(|etag| format!("W/{etag}"));
    if let Some(weak) = weak.and_then(|etag| HeaderValue::from_str(&etag).ok()) {
        parts.headers.insert(header::ETAG, weak);
    }
    if !head {
        parts.headers.insert(header::CONTENT_LENGTH, HeaderValue::from(compressed.len()));
    }
    Response::from_parts(parts, Body::from(compressed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn negotiation_follows_the_servers_preference_and_the_requests_weights() {
        assert_eq!(negotiate(Some("gzip, deflate, br")), Some(Algorithm::Brotli));
        assert_eq!(negotiate(Some("gzip, deflate")), Some(Algorithm::Gzip));
        assert_eq!(negotiate(Some("deflate")), Some(Algorithm::Deflate));
        assert_eq!(negotiate(Some("br;q=0, gzip;q=0.5")), Some(Algorithm::Gzip));
        assert_eq!(negotiate(Some("*;q=0.1, br;q=0")), Some(Algorithm::Gzip));
        assert_eq!(negotiate(Some("identity")), None);
        assert_eq!(negotiate(Some("")), None);
        assert_eq!(negotiate(None), None);
        assert_eq!(negotiate(Some("gzip;q=2")), None, "a weight above one is malformed");
        assert_eq!(negotiate(Some("gzip;q=0.1234")), None, "so is one with four decimals");
        assert_eq!(negotiate(Some("GZIP ; q=1")), Some(Algorithm::Gzip));
    }

    #[test]
    fn only_text_like_types_are_compressible() {
        for kind in ["text/html; charset=utf-8", "application/json", "application/javascript", "image/svg+xml", "application/wasm", "application/ld+json", "application/atom+xml"] {
            assert!(compressible(kind), "{kind}");
        }
        for kind in ["image/png", "application/octet-stream", "font/woff2", "video/mp4"] {
            assert!(!compressible(kind), "{kind}");
        }
    }

    fn response(body: &str, length_known: bool) -> Response {
        let mut response = Response::new(Body::from(body.to_owned()));
        response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
        response.headers_mut().insert(header::ETAG, HeaderValue::from_static("\"abc\""));
        if length_known {
            response.headers_mut().insert(header::CONTENT_LENGTH, HeaderValue::from(body.len()));
        }
        response
    }

    #[tokio::test]
    async fn a_large_text_body_is_compressed_and_round_trips() {
        let text = "{\"row\":\"the same row again\"}".repeat(200);
        for (accept, encoding) in [("br", "br"), ("gzip", "gzip"), ("deflate", "deflate")] {
            let answer = transform(Some(accept), false, response(&text, true)).await;
            assert_eq!(answer.headers()[header::CONTENT_ENCODING], encoding);
            assert_eq!(answer.headers()[header::VARY], "Accept-Encoding");
            assert_eq!(answer.headers()[header::ETAG], "W/\"abc\"", "a compressed answer's tag is weak");
            let bytes = axum::body::to_bytes(answer.into_body(), usize::MAX).await.unwrap();
            assert!(bytes.len() < text.len() / 4);
            let mut decoded = String::new();
            match encoding {
                "br" => drop(brotli::Decompressor::new(&bytes[..], 4096).read_to_string(&mut decoded).unwrap()),
                "gzip" => drop(flate2::read::GzDecoder::new(&bytes[..]).read_to_string(&mut decoded).unwrap()),
                _ => drop(flate2::read::ZlibDecoder::new(&bytes[..]).read_to_string(&mut decoded).unwrap()),
            }
            assert_eq!(decoded, text);
        }
    }

    #[tokio::test]
    async fn small_bodies_unknown_lengths_other_types_and_declared_encodings_are_left_alone() {
        let text = "x".repeat(4000);
        // Under 1024 bytes: not compressed, but the choice depends on the request's header.
        let small = transform(Some("gzip"), false, response("tiny", true)).await;
        assert!(small.headers().get(header::CONTENT_ENCODING).is_none());
        assert_eq!(small.headers()[header::VARY], "Accept-Encoding");
        // A request that accepts nothing we have.
        let none = transform(Some("identity"), false, response(&text, true)).await;
        assert!(none.headers().get(header::CONTENT_ENCODING).is_none());
        assert_eq!(none.headers()[header::VARY], "Accept-Encoding");
        // An image, a stream of unknown length, a no-transform answer, a range, a declared encoding.
        let mut image = response(&text, true);
        image.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
        assert!(transform(Some("gzip"), false, image).await.headers().get(header::CONTENT_ENCODING).is_none());
        let mut stream = Response::new(Body::from_stream(futures_util::stream::iter([Ok::<_, std::io::Error>(axum::body::Bytes::from(text.clone()))])));
        stream.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
        assert!(transform(Some("gzip"), false, stream).await.headers().get(header::CONTENT_ENCODING).is_none());
        let mut untouched = response(&text, true);
        untouched.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("public, no-transform"));
        assert!(transform(Some("gzip"), false, untouched).await.headers().get(header::CONTENT_ENCODING).is_none());
        let mut partial = response(&text, true);
        *partial.status_mut() = StatusCode::PARTIAL_CONTENT;
        assert!(transform(Some("gzip"), false, partial).await.headers().get(header::CONTENT_ENCODING).is_none());
        let mut declared = response(&text, true);
        declared.headers_mut().insert(header::CONTENT_ENCODING, HeaderValue::from_static("zstd"));
        assert_eq!(transform(Some("gzip"), false, declared).await.headers()[header::CONTENT_ENCODING], "zstd");
        let mut identity = response(&text, true);
        identity.headers_mut().insert(header::CONTENT_ENCODING, HeaderValue::from_static("identity"));
        assert!(transform(Some("gzip"), false, identity).await.headers().get(header::CONTENT_ENCODING).is_none());
    }

    #[tokio::test]
    async fn a_head_answer_states_the_encoding_without_a_body() {
        let text = "y".repeat(4000);
        let answer = transform(Some("gzip"), true, response(&text, true)).await;
        assert_eq!(answer.headers()[header::CONTENT_ENCODING], "gzip");
        assert!(answer.headers().get(header::CONTENT_LENGTH).is_none());
    }

    #[tokio::test]
    async fn answers_that_carry_a_credential_are_never_compressed() {
        use axum::{Router, routing::get};
        use tower::ServiceExt;
        let text = "{\"token\":\"the same row again\"}".repeat(200);
        let body = move || {
            let text = text.clone();
            async move { ([(header::CONTENT_TYPE, "application/json")], text) }
        };
        let app = Router::new()
            .route("/plain", get(body.clone()))
            .route("/api/auth/session", get(body.clone()))
            .route("/oauth/token", get(body.clone()))
            .route("/cookie", get(|| async { ([(header::CONTENT_TYPE, "application/json"), (header::SET_COOKIE, "a=b")], "x".repeat(4000)) }))
            .layer(axum::middleware::from_fn(layer));
        for (path, compressed) in [("/plain", true), ("/api/auth/session", false), ("/oauth/token", false), ("/cookie", false)] {
            let answer = app.clone().oneshot(Request::get(path).header(header::ACCEPT_ENCODING, "gzip").body(Body::empty()).unwrap()).await.unwrap();
            assert_eq!(answer.headers().contains_key(header::CONTENT_ENCODING), compressed, "{path}");
        }
    }
}
