//! HTTP transport to an OpenCode server: authenticated requests with bounded bodies, no redirects, and the
//! server-sent event stream. The base address is whatever the instance is configured with (a spawned server is
//! always loopback), the password is sent only to it, and nothing a server answers can move a request elsewhere.
//! Mirrors `OpenCode2Client.ts` (2.x, `@opencode/client`) and the 1.x SDK client's transport (`opencodeRuntime.ts`).
use base64::Engine;
use reqwest::{Method, header::HeaderValue};
use serde_json::Value;
use std::time::Duration;

/// A response body is read up to this many bytes; a longer one is a failure, not a truncation.
const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;
/// One event frame of the stream, up to this many bytes.
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
/// How long any single non-stream request may take unless its caller bounds it tighter.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// OpenCode accepts only HTTP Basic auth, always with this user name.
const USERNAME: &str = "opencode";

/// A failed call. None of its text carries the server address or the password.
#[derive(Debug, Clone, PartialEq)]
pub enum ApiError {
    /// The server could not be reached, or the connection broke.
    Transport(String),
    /// The server answered with a failure status: its error tag (2.x tags every error), status and message.
    Status { status: u16, tag: Option<String>, message: String },
    /// The answer was not what the call expects.
    Decode(String),
    /// A body or frame over its bound.
    TooLarge,
}
impl ApiError {
    /// The 2.x error tag (`SessionNotFoundError`, `ConflictError`, …).
    pub fn tag(&self) -> Option<&str> {
        match self {
            Self::Status { tag, .. } => tag.as_deref(),
            _ => None,
        }
    }
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Status { status, .. } => Some(*status),
            _ => None,
        }
    }
    /// A failure to reach the server, as opposed to an answer from it.
    pub fn is_transport(&self) -> bool {
        matches!(self, Self::Transport(_))
    }
}
impl std::fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(detail) => write!(formatter, "Could not reach the OpenCode server: {detail}"),
            Self::Status { status, tag, message } => match (tag, message.is_empty()) {
                (Some(tag), false) => write!(formatter, "{tag}: {message}"),
                (None, false) => write!(formatter, "OpenCode returned HTTP {status}: {message}"),
                _ => write!(formatter, "OpenCode returned HTTP {status}."),
            },
            Self::Decode(detail) => write!(formatter, "OpenCode sent an unreadable answer: {detail}"),
            Self::TooLarge => write!(formatter, "OpenCode sent more than T3 Code reads."),
        }
    }
}
impl std::error::Error for ApiError {}

/// One server's HTTP API. Cloning shares the connection pool.
#[derive(Clone)]
pub struct Api {
    base: String,
    authorization: Option<HeaderValue>,
    http: reqwest::Client,
    /// 1.x scopes a request to a directory with a header, and a GET also carries it as a `directory` query.
    directory: Option<String>,
}
impl std::fmt::Debug for Api {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Api { .. }")
    }
}
impl Api {
    /// `password` is encoded as UTF-8 (OpenCode decodes it so; `btoa` would reject non-ASCII).
    pub fn new(base_url: &str, password: Option<&str>) -> Result<Self, ApiError> {
        let parsed = url::Url::parse(base_url.trim()).map_err(|_| ApiError::Decode("the server URL is not valid".into()))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(ApiError::Decode("the server URL is not an http:// or https:// URL".into()));
        }
        let authorization = password.filter(|password| !password.is_empty()).map(|password| {
            let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{USERNAME}:{password}"));
            let mut value = HeaderValue::from_str(&format!("Basic {encoded}")).expect("base64 is a valid header value");
            value.set_sensitive(true);
            value
        });
        // A redirect would carry the password to wherever it points; a proxy would route a loopback call off the machine.
        let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).no_proxy().connect_timeout(CONNECT_TIMEOUT).build().map_err(|error| ApiError::Transport(error.to_string()))?;
        Ok(Self { base: parsed.as_str().trim_end_matches('/').to_owned(), authorization, http, directory: None })
    }
    pub fn base(&self) -> &str {
        &self.base
    }
    /// The same server, with every request scoped to `directory` (1.x's `x-opencode-directory`).
    pub fn for_directory(&self, directory: &str) -> Self {
        Self { directory: Some(directory.to_owned()), ..self.clone() }
    }
    fn request(&self, method: Method, path: &str, query: &[(String, String)]) -> reqwest::RequestBuilder {
        let mut request = self.http.request(method.clone(), format!("{}{path}", self.base));
        if !query.is_empty() {
            request = request.query(query);
        }
        if let Some(directory) = &self.directory {
            let encoded = url::form_urlencoded::byte_serialize(directory.as_bytes()).collect::<String>().replace('+', "%20");
            if let Ok(value) = HeaderValue::from_str(&encoded) {
                request = request.header("x-opencode-directory", value);
            }
            if method == Method::GET && !query.iter().any(|(name, _)| name == "directory") {
                request = request.query(&[("directory", directory.as_str())]);
            }
        }
        if let Some(authorization) = &self.authorization {
            request = request.header(reqwest::header::AUTHORIZATION, authorization.clone());
        }
        request
    }
    /// One call: the JSON body it answers (`Null` for an empty one), or the failure. `limit` bounds the whole call.
    pub async fn call(&self, method: Method, path: &str, query: &[(String, String)], body: Option<&Value>, limit: Option<Duration>) -> Result<Value, ApiError> {
        let mut request = self.request(method, path, query).timeout(limit.unwrap_or(REQUEST_TIMEOUT));
        if let Some(body) = body {
            request = request.header(reqwest::header::CONTENT_TYPE, "application/json").body(body.to_string());
        }
        let response = request.send().await.map_err(transport)?;
        let status = response.status();
        let (bytes, truncated) = read_bounded(response, MAX_BODY_BYTES).await?;
        if truncated {
            return Err(ApiError::TooLarge);
        }
        if !status.is_success() {
            return Err(status_error(status.as_u16(), &bytes));
        }
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&bytes).map_err(|_| ApiError::Decode("the body is not JSON".into()))
    }
    /// Like [`Api::call`] for an answer that is not JSON (1.x answers its web UI as HTML on a path it does not serve):
    /// the status, content type and the body as text.
    pub async fn probe(&self, path: &str, limit: Duration) -> Result<(u16, Option<String>, String), ApiError> {
        let response = self.request(Method::GET, path, &[]).timeout(limit).send().await.map_err(transport)?;
        let status = response.status().as_u16();
        let content_type = response.headers().get(reqwest::header::CONTENT_TYPE).and_then(|value| value.to_str().ok()).map(str::to_owned);
        let (bytes, _) = read_bounded(response, 1024 * 1024).await?;
        Ok((status, content_type, String::from_utf8_lossy(&bytes).into_owned()))
    }
    /// Subscribes to `path`'s event stream. Resolves once the server accepted the subscription: the stream is
    /// volatile, so callers subscribe before they start the work whose events they need.
    pub async fn subscribe(&self, path: &str) -> Result<EventStream, ApiError> {
        let response = self.request(Method::GET, path, &[]).header(reqwest::header::ACCEPT, "text/event-stream").send().await.map_err(transport)?;
        let status = response.status();
        if !status.is_success() {
            let (bytes, _) = read_bounded(response, MAX_BODY_BYTES).await.unwrap_or_default();
            return Err(status_error(status.as_u16(), &bytes));
        }
        Ok(EventStream { response, buffer: Vec::new() })
    }
}

fn transport(error: reqwest::Error) -> ApiError {
    // The error's own text names the URL; only its kind is kept.
    let kind = if error.is_timeout() {
        "the request timed out"
    } else if error.is_connect() {
        "the connection was refused"
    } else if error.is_redirect() {
        "the server redirected the request"
    } else if error.is_body() || error.is_decode() {
        "the connection broke while reading"
    } else {
        "the request failed"
    };
    ApiError::Transport(kind.into())
}
fn status_error(status: u16, body: &[u8]) -> ApiError {
    let parsed: Option<Value> = serde_json::from_slice(body).ok();
    let tag = parsed.as_ref().and_then(|body| body["_tag"].as_str()).map(str::to_owned);
    let message = parsed.as_ref().and_then(|body| body["message"].as_str()).unwrap_or("").chars().take(500).collect();
    ApiError::Status { status, tag, message }
}
/// The body up to `max` bytes and whether more followed.
async fn read_bounded(mut response: reqwest::Response, max: usize) -> Result<(Vec<u8>, bool), ApiError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(transport)? {
        let room = max - bytes.len();
        if chunk.len() > room {
            bytes.extend_from_slice(&chunk[..room]);
            return Ok((bytes, true));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok((bytes, false))
}

/// A server-sent event stream: each frame's `data` read as JSON. Comment lines (`: heartbeat`) are skipped.
pub struct EventStream {
    response: reqwest::Response,
    buffer: Vec<u8>,
}
impl EventStream {
    /// The next event, `Ok(None)` when the server closed the stream. A frame whose data is not JSON is skipped,
    /// as a build that cannot decode an event skips it.
    pub async fn next(&mut self) -> Result<Option<Value>, ApiError> {
        loop {
            while let Some(frame) = take_frame(&mut self.buffer) {
                if let Some(event) = frame_data(&frame) {
                    return Ok(Some(event));
                }
            }
            if self.buffer.len() > MAX_FRAME_BYTES {
                return Err(ApiError::TooLarge);
            }
            match self.response.chunk().await.map_err(transport)? {
                Some(chunk) => self.buffer.extend_from_slice(&chunk),
                None => return Ok(None),
            }
        }
    }
}
/// A frame ends at a blank line (`\n\n`, `\r\n\r\n`).
fn take_frame(buffer: &mut Vec<u8>) -> Option<Vec<u8>> {
    let end = (0..buffer.len()).find_map(|at| {
        if buffer[at..].starts_with(b"\n\n") {
            Some((at, 2))
        } else if buffer[at..].starts_with(b"\r\n\r\n") {
            Some((at, 4))
        } else {
            None
        }
    })?;
    let frame = buffer[..end.0].to_vec();
    buffer.drain(..end.0 + end.1);
    Some(frame)
}
fn frame_data(frame: &[u8]) -> Option<Value> {
    let text = String::from_utf8_lossy(frame);
    let data: Vec<&str> = text.lines().filter_map(|line| line.strip_prefix("data:")).map(|data| data.strip_prefix(' ').unwrap_or(data)).collect();
    if data.is_empty() {
        return None;
    }
    serde_json::from_str(&data.join("\n")).ok()
}

/// Streams every item of a cursor-paged list (`paginate`): the first request carries the caller's query (including
/// `order`), later ones only the cursor, because OpenCode rejects a cursor combined with `order`.
pub async fn paginate(api: &Api, path: &str, first: Vec<(String, String)>, limit: Option<Duration>) -> Result<Vec<Value>, ApiError> {
    let mut items = vec![];
    let mut query = first;
    loop {
        let page = api.call(Method::GET, path, &query, None, limit).await?;
        let data = page["data"].as_array().cloned().unwrap_or_default();
        let next = page["cursor"]["next"].as_str().map(str::to_owned);
        let empty = data.is_empty();
        items.extend(data);
        match next {
            Some(cursor) if !empty => query = vec![("cursor".into(), cursor)],
            _ => return Ok(items),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frames_split_at_blank_lines_and_skip_comments() {
        let mut buffer = b": heartbeat\n\ndata: {\"type\":\"a\"}\n\ndata: {\"type\"".to_vec();
        let mut seen = vec![];
        while let Some(frame) = take_frame(&mut buffer) {
            if let Some(event) = frame_data(&frame) {
                seen.push(event);
            }
        }
        assert_eq!(seen, vec![serde_json::json!({"type":"a"})]);
        assert_eq!(buffer, b"data: {\"type\"");
    }
    #[test]
    fn multiline_data_is_joined_and_crlf_frames_split() {
        let mut buffer = b"data: {\"a\":\ndata: 1}\r\n\r\n".to_vec();
        let frame = take_frame(&mut buffer).unwrap();
        assert_eq!(frame_data(&frame), Some(serde_json::json!({"a":1})));
    }
    #[test]
    fn errors_name_neither_the_address_nor_the_password() {
        let api = Api::new("http://127.0.0.1:9", Some("secret-pass")).unwrap();
        assert!(!format!("{api:?}").contains("secret"));
        let error = status_error(404, br#"{"_tag":"SessionNotFoundError","message":"gone"}"#);
        assert_eq!(error.tag(), Some("SessionNotFoundError"));
        assert!(!error.to_string().contains("127.0.0.1"));
    }
}
