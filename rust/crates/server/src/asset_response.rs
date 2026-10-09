//! Response headers and bodies for signed asset downloads (the header policy of
//! apps/server/src/http.ts). Everything served from the attachment store is user or agent
//! supplied, so the policy is deliberately restrictive: downloads are sandboxed and typed
//! as octet-stream unless the signed mime type is demonstrably not a renderable document.
use crate::{asset_access::ResolvedAsset, attachment_store::mime_type_for_path};
use axum::{
    body::Body,
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use regex::Regex;
use std::sync::LazyLock;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

const SVG_CONTENT_SECURITY_POLICY: &str = "default-src 'none'; style-src 'unsafe-inline'; sandbox";
// HTML previews are agent output, not the app. The sandbox gives the document an opaque
// origin: scripts run, but same-origin cookies, storage, and API calls are out of reach.
// No modals: agent HTML can open without a click and must not raise blocking dialogs.
const HTML_CONTENT_SECURITY_POLICY: &str = "sandbox allow-scripts allow-forms allow-popups";

static MIME_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_!#$&^.+-]+/[A-Za-z0-9_!#$&^.+-]+$").unwrap());
static RENDERABLE_MIME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^text/html$|/xml(?:$|-)|\+xml$)").unwrap());
static MEDIA_MIME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(?:audio|video)/").unwrap());
static BYTE_RANGE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^bytes=([0-9]*)-([0-9]*)$").unwrap());

// Types a browser may render as a document if a proxy strips the disposition header.
fn is_safe_download_mime(mime_type: &str) -> bool {
    MIME_TOKEN.is_match(mime_type)
        && !RENDERABLE_MIME.is_match(&t3_contracts::trim_wire_string(mime_type).to_lowercase())
}
fn is_safe_inline_media_mime(mime_type: &str) -> bool {
    MIME_TOKEN.is_match(mime_type) && MEDIA_MIME.is_match(mime_type)
}
fn is_safe_inline_document_mime(mime_type: &str) -> bool {
    matches!(
        mime_type.to_lowercase().as_str(),
        "application/pdf" | "text/html"
    )
}

/// RFC 6266 disposition with an ASCII fallback name plus a UTF-8 `filename*`.
pub fn download_content_disposition(file_name: Option<&str>) -> String {
    let Some(file_name) = file_name else {
        return "attachment".into();
    };
    let sanitized: String = file_name
        .chars()
        .map(|c| {
            if c.is_control() || c == '"' || c == '\\' {
                '_'
            } else {
                c
            }
        })
        .collect();
    // The source replaces UTF-16 code units, so an astral character becomes two underscores.
    let ascii: String = sanitized
        .chars()
        .flat_map(|c| {
            let replaced = if (' '..='~').contains(&c) { c } else { '_' };
            std::iter::repeat_n(
                replaced,
                if replaced == '_' && c != '_' {
                    c.len_utf16()
                } else {
                    1
                },
            )
        })
        .collect();
    if ascii == sanitized {
        return format!("attachment; filename=\"{ascii}\"");
    }
    let extended = crate::asset_access::encode_uri_component(&sanitized)
        .replace('\'', "%27")
        .replace('(', "%28")
        .replace(')', "%29")
        .replace('*', "%2A");
    format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{extended}")
}

pub fn asset_response_headers(
    file_path: &str,
    download: bool,
    file_name: Option<&str>,
    mime_type: Option<&str>,
) -> Vec<(&'static str, String)> {
    let lower_path = file_path.to_lowercase();
    let inline_mime =
        mime_type.map(|mime| t3_contracts::trim_wire_string(mime.split(';').next().unwrap_or("")));
    let mut headers = vec![
        ("Cache-Control", "private, max-age=3600".to_owned()),
        ("X-Content-Type-Options", "nosniff".to_owned()),
    ];
    if download {
        headers.push((
            "Content-Disposition",
            download_content_disposition(file_name),
        ));
        headers.push((
            "Content-Security-Policy",
            "default-src 'none'; sandbox".into(),
        ));
        headers.push((
            "Content-Type",
            match mime_type {
                Some(mime) if is_safe_download_mime(mime) => mime.to_owned(),
                _ => "application/octet-stream".into(),
            },
        ));
    } else if let Some(mime) = inline_mime.filter(|mime| is_safe_inline_media_mime(mime)) {
        headers.push(("Content-Type", mime.to_owned()));
    } else if let Some(mime) = inline_mime.filter(|mime| is_safe_inline_document_mime(mime)) {
        if mime.eq_ignore_ascii_case("text/html") {
            headers.push(("Content-Type", "text/html; charset=utf-8".into()));
            headers.push((
                "Content-Security-Policy",
                HTML_CONTENT_SECURITY_POLICY.into(),
            ));
        } else {
            headers.push(("Content-Type", "application/pdf".into()));
        }
    } else if lower_path.ends_with(".html") || lower_path.ends_with(".htm") {
        headers.push(("Content-Type", "text/html; charset=utf-8".into()));
        headers.push((
            "Content-Security-Policy",
            HTML_CONTENT_SECURITY_POLICY.into(),
        ));
    }
    if !download && lower_path.ends_with(".svg") {
        headers.push((
            "Content-Security-Policy",
            SVG_CONTENT_SECURITY_POLICY.into(),
        ));
    }
    headers
}

enum ByteRange {
    Unsatisfiable,
    Range {
        offset: u64,
        length: u64,
        content_range: String,
    },
}
/// A single byte range for native media readers; unsupported syntax uses the full file.
fn byte_range(header: &str, size: u64) -> Option<ByteRange> {
    let captures = BYTE_RANGE.captures(t3_contracts::trim_wire_string(header))?;
    let (first, last) = (&captures[1], &captures[2]);
    if first.is_empty() && last.is_empty() {
        return None;
    }
    let parse = |digits: &str| (!digits.is_empty()).then(|| digits.parse::<u128>());
    let (first, last) = (
        parse(first).transpose().ok()?,
        parse(last).transpose().ok()?,
    );
    if let (Some(first), Some(last)) = (first, last) {
        if last < first {
            return None;
        }
    }
    let size = size as u128;
    if size == 0 || first.is_some_and(|first| first >= size) || (first.is_none() && last == Some(0))
    {
        return Some(ByteRange::Unsatisfiable);
    }
    let start = first.unwrap_or_else(|| {
        if last.unwrap() >= size {
            0
        } else {
            size - last.unwrap()
        }
    });
    let end = match (first, last) {
        (Some(_), Some(last)) if last < size => last,
        _ => size - 1,
    };
    Some(ByteRange::Range {
        offset: start as u64,
        length: (end - start + 1) as u64,
        content_range: format!("bytes {start}-{end}/{size}"),
    })
}

fn apply(headers: &mut HeaderMap, pairs: &[(&str, String)]) {
    for (name, value) in pairs {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }
}
fn text(status: StatusCode, body: &'static str) -> Response {
    (status, body).into_response()
}

pub async fn file_response(
    asset: &ResolvedAsset,
    range_header: Option<&str>,
    if_range_header: Option<&str>,
    head: bool,
) -> Response {
    let ResolvedAsset::File {
        path,
        download,
        file_name,
        mime_type,
        file: open,
    } = asset
    else {
        return text(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error");
    };
    let path_text = path.to_string_lossy();
    let mut pairs: Vec<(&'static str, String)> = asset_response_headers(
        &path_text,
        *download,
        file_name.as_deref(),
        mime_type.as_deref(),
    );
    // A media file is read through the descriptor that was checked at resolution, never its path.
    let metadata = match open {
        Some(open) => open.file.metadata(),
        None => tokio::fs::metadata(path).await,
    };
    let Ok(metadata) = metadata else {
        return text(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error");
    };
    let size = metadata.len();
    let content_type = |pairs: &[(&'static str, String)]| {
        pairs
            .iter()
            .find(|(name, _)| *name == "Content-Type")
            .map(|(_, value)| value.clone())
    };
    let is_media = content_type(&pairs).is_some_and(|value| MEDIA_MIME.is_match(&value));
    let mut status = StatusCode::OK;
    let (mut offset, mut length) = (0, size);
    let set = |pairs: &mut Vec<(&'static str, String)>, name: &'static str, value: String| {
        pairs.retain(|(existing, _)| *existing != name);
        pairs.push((name, value));
    };
    if is_media {
        // Media is never cached beyond the signed URL that granted it.
        set(&mut pairs, "Cache-Control", "private, no-store".into());
        set(&mut pairs, "Accept-Ranges", "bytes".into());
        // If-Range needs a validator we do not issue, so a full response is the safe answer.
        if !head && if_range_header.is_none() {
            match range_header.and_then(|header| byte_range(header, size)) {
                Some(ByteRange::Unsatisfiable) => {
                    set(&mut pairs, "Content-Range", format!("bytes */{size}"));
                    let mut response = Response::new(Body::empty());
                    *response.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
                    apply(response.headers_mut(), &pairs);
                    return response;
                }
                Some(ByteRange::Range {
                    offset: start,
                    length: count,
                    content_range,
                }) => {
                    status = StatusCode::PARTIAL_CONTENT;
                    (offset, length) = (start, count);
                    set(&mut pairs, "Content-Range", content_range);
                }
                None => {}
            }
        }
    }
    if content_type(&pairs).is_none() {
        set(
            &mut pairs,
            "Content-Type",
            mime_type_for_path(&path_text).into(),
        );
    }
    set(&mut pairs, "Content-Length", length.to_string());
    if let Ok(modified) = metadata.modified() {
        let modified: chrono::DateTime<chrono::Utc> = modified.into();
        set(
            &mut pairs,
            "Last-Modified",
            modified.format("%a, %d %b %Y %H:%M:%S GMT").to_string(),
        );
        set(
            &mut pairs,
            "ETag",
            format!("W/\"{:x}-{:x}\"", size, modified.timestamp_millis()),
        );
    }
    let body = if head || length == 0 {
        Body::empty()
    } else {
        let opened = match open {
            Some(open) => open.file.try_clone().map(tokio::fs::File::from_std),
            None => tokio::fs::File::open(path).await,
        };
        let Ok(mut file) = opened else {
            return text(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error");
        };
        if offset > 0 && file.seek(std::io::SeekFrom::Start(offset)).await.is_err() {
            return text(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error");
        }
        Body::from_stream(futures_util::stream::unfold(
            (file, length),
            |(mut file, remaining)| async move {
                if remaining == 0 {
                    return None;
                }
                let mut buffer = vec![0; remaining.min(64 * 1024) as usize];
                match file.read(&mut buffer).await {
                    Ok(0) => None,
                    Ok(read) => {
                        buffer.truncate(read);
                        Some((
                            Ok::<_, std::io::Error>(axum::body::Bytes::from(buffer)),
                            (file, remaining - read as u64),
                        ))
                    }
                    Err(error) => Some((Err(error), (file, 0))),
                }
            },
        ))
    };
    let mut response = Response::new(body);
    *response.status_mut() = status;
    apply(response.headers_mut(), &pairs);
    response
}

/// An image a tool returned inline: bytes the server decoded, typed by the raster mime it checked.
pub fn bytes_response(bytes: &[u8], mime_type: &str) -> Response {
    let mut response = Response::new(Body::from(bytes.to_vec()));
    apply(
        response.headers_mut(),
        &[
            ("Content-Type", mime_type.to_owned()),
            ("Cache-Control", "private, max-age=3600".into()),
            ("X-Content-Type-Options", "nosniff".into()),
        ],
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attachment_service::testing::access_fixture;
    use axum::body::to_bytes;
    use serde_json::{Value, json};

    #[test]
    fn original_header_policy_and_dispositions_agree_on_generated_cases() {
        let fixture = access_fixture();
        for row in fixture["dispositions"].as_array().unwrap() {
            assert_eq!(
                download_content_disposition(row["fileName"].as_str()),
                row["value"].as_str().unwrap(),
                "{}",
                row["fileName"]
            );
        }
        let cases = fixture["headerCases"].as_array().unwrap();
        assert!(cases.len() > 100);
        for case in cases {
            let options = &case["options"];
            let actual: serde_json::Map<String, Value> = asset_response_headers(
                case["path"].as_str().unwrap(),
                options["download"] == true,
                options["fileName"].as_str(),
                options["mimeType"].as_str(),
            )
            .into_iter()
            .map(|(name, value)| (name.to_owned(), json!(value)))
            .collect();
            assert_eq!(Value::Object(actual), case["headers"], "{case}");
        }
    }

    #[test]
    fn byte_ranges_follow_the_media_reader_rules() {
        let range = |header: &str, size: u64| match byte_range(header, size) {
            None => "full".to_owned(),
            Some(ByteRange::Unsatisfiable) => "416".to_owned(),
            Some(ByteRange::Range { content_range, .. }) => content_range,
        };
        assert_eq!(range("bytes=0-1", 3), "bytes 0-1/3");
        assert_eq!(range("bytes=1-", 3), "bytes 1-2/3");
        assert_eq!(range("bytes=-2", 3), "bytes 1-2/3");
        assert_eq!(range("bytes=-10", 3), "bytes 0-2/3");
        assert_eq!(range("bytes=1-99", 3), "bytes 1-2/3");
        assert_eq!(range(" BYTES=0-0 ", 3), "bytes 0-0/3");
        assert_eq!(range("bytes=3-", 3), "416");
        assert_eq!(range("bytes=-0", 3), "416");
        assert_eq!(range("bytes=0-0", 0), "416");
        assert_eq!(range("bytes=2-1", 3), "full");
        assert_eq!(range("bytes=-", 3), "full");
        assert_eq!(range("bytes=0-1,3-4", 3), "full");
        assert_eq!(range("items=0-1", 3), "full");
    }

    async fn serve(
        name: &str,
        bytes: &[u8],
        asset: impl FnOnce(PathBuf) -> ResolvedAsset,
        range: Option<&str>,
        head: bool,
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        let response = file_response(&asset(path), range, None, head).await;
        let (parts, body) = response.into_parts();
        (
            parts.status,
            parts.headers,
            to_bytes(body, usize::MAX).await.unwrap().to_vec(),
        )
    }
    use std::path::PathBuf;
    fn file(download: bool, mime: Option<&str>) -> impl FnOnce(PathBuf) -> ResolvedAsset + '_ {
        move |path| ResolvedAsset::File {
            path,
            download,
            file_name: Some("n.bin".into()),
            mime_type: mime.map(str::to_owned),
            file: None,
        }
    }

    #[tokio::test]
    async fn images_download_and_media_responses_carry_the_signed_policy() {
        let (status, headers, body) =
            serve("a.png", &[1, 2, 3], file(false, None), None, false).await;
        assert_eq!((status, body), (StatusCode::OK, vec![1, 2, 3]));
        assert_eq!(headers["content-type"], "image/png");
        assert_eq!(headers["content-length"], "3");
        assert_eq!(headers["cache-control"], "private, max-age=3600");
        assert_eq!(headers["x-content-type-options"], "nosniff");
        assert!(headers.contains_key("etag") && headers.contains_key("last-modified"));
        assert!(!headers.contains_key("accept-ranges"));

        let (_, headers, body) = serve(
            "a.pdf",
            b"pdf",
            file(true, Some("application/pdf")),
            None,
            false,
        )
        .await;
        assert_eq!(body, b"pdf");
        assert_eq!(
            headers["content-disposition"],
            "attachment; filename=\"n.bin\""
        );
        assert_eq!(
            headers["content-security-policy"],
            "default-src 'none'; sandbox"
        );
        assert_eq!(headers["content-type"], "application/pdf");

        let (_, headers, _) =
            serve("a.html", b"<b>", file(true, Some("text/html")), None, false).await;
        assert_eq!(headers["content-type"], "application/octet-stream");
        let (_, headers, _) = serve("a.svg", b"<svg/>", file(false, None), None, false).await;
        assert!(
            headers["content-security-policy"]
                .to_str()
                .unwrap()
                .contains("sandbox")
        );

        let (status, headers, body) = serve(
            "v.mp4",
            b"0123456789",
            file(false, Some("video/mp4")),
            Some("bytes=2-5"),
            false,
        )
        .await;
        assert_eq!(
            (status, body.as_slice()),
            (StatusCode::PARTIAL_CONTENT, b"2345".as_slice())
        );
        assert_eq!(headers["content-range"], "bytes 2-5/10");
        assert_eq!(headers["content-length"], "4");
        assert_eq!(headers["cache-control"], "private, no-store");
        assert_eq!(headers["accept-ranges"], "bytes");
        let (status, headers, body) = serve(
            "v.mp4",
            b"0123456789",
            file(false, Some("video/mp4")),
            Some("bytes=10-"),
            false,
        )
        .await;
        assert_eq!((status, body.len()), (StatusCode::RANGE_NOT_SATISFIABLE, 0));
        assert_eq!(headers["content-range"], "bytes */10");
        let (status, _, body) = serve(
            "v.mp4",
            b"0123456789",
            file(false, Some("video/mp4")),
            Some("bytes=0-1"),
            true,
        )
        .await;
        assert_eq!((status, body.len()), (StatusCode::OK, 0));
        // A range on a non-media file is ignored.
        let (status, _, body) = serve(
            "a.png",
            &[1, 2, 3],
            file(false, None),
            Some("bytes=1-1"),
            false,
        )
        .await;
        assert_eq!((status, body.len()), (StatusCode::OK, 3));
    }
}
