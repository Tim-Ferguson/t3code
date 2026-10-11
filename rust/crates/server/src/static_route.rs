//! The web app, served beside the API (`handleStaticAndDevRequest` in `apps/server/src/http.ts`): a file of the built bundle, the
//! bundle's `index.html` for a path with no extension (a client-side route) or a file that is not there, and, for a server started
//! with a dev URL, a redirect to that dev server. Files carry validators (a weak `ETag` and `Last-Modified`, answered `304` when
//! the client's copy is current) and a cache policy: build assets named in the bundle's `.vite/manifest.json` are immutable, the
//! rest revalidate.
//!
//! Not ported: the response compression the original's router applies to every response (`HttpMiddleware.compression`).
use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use std::{
    collections::HashSet,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
    time::UNIX_EPOCH,
};

/// `DEV_PROXIED_PATH_PREFIXES`: the paths the web dev server proxies back to this one, which a dev catch-all must 404.
const DEV_PROXIED_PATH_PREFIXES: [&str; 5] = ["/api", "/oauth", "/.well-known", "/ws", "/mcp"];
const LOOPBACK_HOSTNAMES: [&str; 3] = ["127.0.0.1", "::1", "localhost"];

pub fn is_dev_proxied_path(pathname: &str) -> bool {
    DEV_PROXIED_PATH_PREFIXES.iter().any(|prefix| pathname == *prefix || pathname.strip_prefix(prefix).is_some_and(|rest| rest.starts_with('/')))
}

pub fn is_loopback_hostname(hostname: &str) -> bool {
    let normalized = hostname.trim().to_lowercase();
    let normalized = normalized.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')).unwrap_or(&normalized);
    LOOPBACK_HOSTNAMES.contains(&normalized)
}

/// `resolveDevRedirectUrl`: the dev URL with the request's path and query.
pub fn resolve_dev_redirect_url(dev_url: &url::Url, path: &str, query: Option<&str>) -> String {
    let mut redirect = dev_url.clone();
    redirect.set_path(path);
    redirect.set_query(query);
    redirect.set_fragment(None);
    redirect.to_string()
}

/// What a started server knows about its web app: the dev server to hand loopback requests to, and the built files.
#[derive(Clone, Default)]
pub struct StaticConfig {
    pub dev_url: Option<url::Url>,
    /// The bundle's `.vite/manifest.json` files, read once.
    immutable: Arc<Mutex<Option<Arc<HashSet<String>>>>>,
}

impl StaticConfig {
    pub fn with_dev_url(dev_url: Option<url::Url>) -> Self {
        Self { dev_url, ..Self::default() }
    }

    /// Read the installed build's manifest once. Unknown files use revalidation.
    fn immutable_assets(&self, root: &Path) -> Arc<HashSet<String>> {
        let mut slot = self.immutable.lock().unwrap();
        slot.get_or_insert_with(|| {
            let files = std::fs::read(root.join(".vite").join("manifest.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .and_then(|manifest| {
                    manifest.as_object().map(|entries| {
                        entries
                            .values()
                            .flat_map(|entry| {
                                let strings = |key: &str| entry.get(key).and_then(|v| v.as_array()).into_iter().flatten().filter_map(|v| v.as_str().map(str::to_owned)).collect::<Vec<_>>();
                                entry.get("file").and_then(|v| v.as_str()).map(str::to_owned).into_iter().chain(strings("css")).chain(strings("assets")).collect::<Vec<_>>()
                            })
                            .collect::<HashSet<_>>()
                    })
                })
                .unwrap_or_default();
            Arc::new(files)
        })
        .clone()
    }
}

#[derive(Default)]
struct Slot(std::sync::OnceLock<StaticConfig>);

/// Installs the dev URL a server was started with (`--dev-url`).
pub fn install(store: &crate::persistence::Store, config: StaticConfig) {
    let _ = store.extension(Slot::default).0.set(config);
}

pub fn current(store: &crate::persistence::Store) -> StaticConfig {
    store.extension(Slot::default).0.get().cloned().unwrap_or_default()
}

fn text(status: StatusCode, body: &'static str) -> Response {
    (status, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], body).into_response()
}

/// The media type of a file by its extension (the `mime` package's table, for the types a web app ships).
fn mime_type(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("html" | "htm") => "text/html",
        Some("js" | "mjs") => "text/javascript",
        Some("css") => "text/css",
        Some("json" | "map") => "application/json",
        Some("webmanifest") => "application/manifest+json",
        Some("wasm") => "application/wasm",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("avif") => "image/avif",
        Some("ico") => "image/x-icon",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        Some("otf") => "font/otf",
        Some("txt") => "text/plain",
        Some("xml") => "application/xml",
        Some("pdf") => "application/pdf",
        Some("mp3") => "audio/mpeg",
        Some("mp4") => "video/mp4",
        Some("webm") => "video/webm",
        _ => "application/octet-stream",
    }
}

/// `Date.prototype.toUTCString`.
fn utc_string(modified_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(modified_ms).unwrap_or_default().format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

fn parse_http_date(value: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc2822(value.trim()).ok().map(|date| date.timestamp_millis())
}

/// Lexical normalization of a relative path (`path.normalize`): `.` and `x/..` fold away, a leading `..` stays.
fn normalize(relative: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in relative.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|last| *last != "..") {
                    parts.pop();
                } else {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

// Deliberately stricter than the original (apps/server/src/http.ts `handleStaticAndDevRequest`), whose check that a file lies inside
// the bundle is on the path's spelling only, so a symlink in the bundle that points outside it is served. Here the resolved file must
// lie inside the resolved bundle; a link that escapes is treated as no file.
async fn open_file(path: &Path, root: &Path) -> Option<(tokio::fs::File, std::fs::Metadata)> {
    // Directories and special files are rejected before opening; the response metadata comes from the handle.
    if !tokio::fs::metadata(path).await.ok()?.is_file() {
        return None;
    }
    let resolved = tokio::fs::canonicalize(path).await.ok()?;
    if !resolved.starts_with(tokio::fs::canonicalize(root).await.ok()?) {
        return None;
    }
    let file = tokio::fs::File::open(&resolved).await.ok()?;
    let info = file.metadata().await.ok()?;
    info.is_file().then_some((file, info))
}

/// The request's response. `root` is the bundle directory (`--assets`, or the one beside the executable).
pub async fn serve(root: Option<PathBuf>, config: StaticConfig, method: &Method, uri: &axum::http::Uri, request_headers: &HeaderMap) -> Response {
    let pathname = uri.path();
    if let Some(dev_url) = &config.dev_url {
        if is_dev_proxied_path(pathname) {
            return text(StatusCode::NOT_FOUND, "Not Found");
        }
        let host = request_headers.get(header::HOST).and_then(|host| host.to_str().ok()).map(|host| host.rsplit_once(':').filter(|(_, port)| port.bytes().all(|b| b.is_ascii_digit()) && !host.ends_with(']')).map_or(host, |(name, _)| name)).unwrap_or_default();
        if is_loopback_hostname(host) {
            let location = resolve_dev_redirect_url(dev_url, pathname, uri.query());
            return (StatusCode::FOUND, [(header::LOCATION, location)]).into_response();
        }
    }
    let Some(static_root) = root else {
        return text(StatusCode::SERVICE_UNAVAILABLE, "No static directory configured and no dev URL set.");
    };
    let static_root = if static_root.is_absolute() { static_root } else { std::env::current_dir().map(|cwd| cwd.join(&static_root)).unwrap_or(static_root) };
    // TODO(original apps/server/src/http.ts `handleStaticAndDevRequest`): the request path is not percent-decoded, so a file whose
    // name needs escaping (`a b.txt` requested as `a%20b.txt`) is never found. Copied as it is.
    let request_path = if pathname == "/" { "/index.html" } else { pathname };
    let raw_relative = request_path.trim_start_matches(['/', '\\']);
    let relative = normalize(raw_relative);
    if relative.is_empty() || raw_relative.starts_with("..") || relative.starts_with("..") || relative.contains('\0') {
        return text(StatusCode::BAD_REQUEST, "Invalid static file path");
    }
    let within = |candidate: &Path| candidate.starts_with(&static_root);
    let mut file_path = static_root.join(&relative);
    if file_path.components().any(|part| matches!(part, Component::ParentDir)) || !within(&file_path) {
        return text(StatusCode::BAD_REQUEST, "Invalid static file path");
    }
    if file_path.extension().is_none() {
        file_path = file_path.join("index.html");
        if !within(&file_path) {
            return text(StatusCode::BAD_REQUEST, "Invalid static file path");
        }
    }
    let opened = match open_file(&file_path, &static_root).await {
        Some(opened) => Some(opened),
        None => {
            file_path = static_root.join("index.html");
            open_file(&file_path, &static_root).await
        }
    };
    let Some((file, info)) = opened else {
        return text(StatusCode::NOT_FOUND, "Not Found");
    };
    let mime = mime_type(&file_path);
    let is_html = mime == "text/html";
    // A hash-like name is not enough: custom static files can use the same naming pattern.
    let relative_path = file_path.strip_prefix(&static_root).map(|path| path.to_string_lossy().replace('\\', "/")).unwrap_or_default();
    static ASSET_NAME: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let hashed = ASSET_NAME.get_or_init(|| regex::Regex::new(r"^assets/.+-[\w-]{8}\.[^/]+$").unwrap());
    let immutable = !is_html && hashed.is_match(&relative_path) && config.immutable_assets(&static_root).contains(&relative_path);
    let mut headers = HeaderMap::new();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(if immutable { "public, max-age=31536000, immutable" } else { "no-cache" }));
    // Deployments can preserve HTML size and mtime while changing its bundle URLs.
    let modified_ms = (!is_html).then(|| info.modified().ok().and_then(|time| time.duration_since(UNIX_EPOCH).ok()).map(|elapsed| elapsed.as_millis() as i64)).flatten();
    let etag = modified_ms.map(|modified| format!("W/\"{:x}-{:x}\"", info.len(), modified));
    if let (Some(etag), Some(modified)) = (&etag, modified_ms) {
        headers.insert(header::ETAG, HeaderValue::from_str(etag).unwrap());
        headers.insert(header::LAST_MODIFIED, HeaderValue::from_str(&utc_string(modified)).unwrap());
    }
    // If-None-Match takes precedence over dates and uses weak comparison for GET/HEAD.
    let if_none_match = request_headers.get(header::IF_NONE_MATCH).and_then(|value| value.to_str().ok());
    let unchanged = match if_none_match {
        Some(list) => list.split(',').any(|candidate| {
            let candidate = candidate.trim();
            candidate == "*" || etag.as_deref().is_some_and(|etag| candidate.strip_prefix("W/").or_else(|| candidate.strip_prefix("w/")).unwrap_or(candidate) == &etag[2..])
        }),
        None => match (request_headers.get(header::IF_MODIFIED_SINCE).and_then(|value| value.to_str().ok()).and_then(parse_http_date), modified_ms) {
            (Some(since), Some(modified)) => modified.div_euclid(1000) * 1000 <= since,
            _ => false,
        },
    };
    if !is_html && unchanged {
        headers.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
        return (StatusCode::NOT_MODIFIED, headers).into_response();
    }
    let content_type = if is_html { "text/html; charset=utf-8" } else { mime };
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(info.len()));
    let body = if method == Method::HEAD { Body::empty() } else { Body::from_stream(tokio_util_stream(file)) };
    (StatusCode::OK, headers, body).into_response()
}

/// The file in 64 KiB chunks, as the original streams it.
fn tokio_util_stream(file: tokio::fs::File) -> impl futures_util::Stream<Item = std::io::Result<Vec<u8>>> {
    use tokio::io::AsyncReadExt;
    futures_util::stream::unfold(file, |mut file| async move {
        let mut buffer = vec![0u8; 65_536];
        match file.read(&mut buffer).await {
            Ok(0) => None,
            Ok(read) => {
                buffer.truncate(read);
                Some((Ok(buffer), file))
            }
            Err(error) => Some((Err(error), file)),
        }
    })
}
