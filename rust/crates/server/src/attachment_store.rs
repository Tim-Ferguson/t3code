//! Attachment ids, paths, and the pending-upload lifecycle. Ports attachmentPaths.ts,
//! attachmentStore.ts, and imageMime.ts; strings stay `&str` so path semantics match
//! node's posix `path` module exactly (the original server never normalizes through the OS).
use regex::{Regex, RegexBuilder};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::LazyLock,
};
use t3_contracts::trim_wire_string;

pub const PENDING_ATTACHMENT_THREAD_SEGMENT: &str = "pending";
const PENDING_ATTACHMENT_MAX_AGE_MS: f64 = 24.0 * 60.0 * 60.0 * 1000.0;
const PARTIAL_UPLOAD_MAX_AGE_MS: f64 = 60.0 * 60.0 * 1000.0;
const THREAD_SEGMENT_MAX_CHARS: usize = 80;
pub const SAFE_IMAGE_FILE_EXTENSIONS: &[&str] = &[
    ".avif", ".bmp", ".gif", ".heic", ".heif", ".ico", ".jpeg", ".jpg", ".png", ".svg", ".tiff",
    ".webp",
];
const ATTACHMENT_FILENAME_EXTENSIONS: &[&str] = &[
    ".avif", ".bmp", ".gif", ".heic", ".heif", ".ico", ".jpeg", ".jpg", ".png", ".svg", ".tiff",
    ".webp", ".bin",
];
const IMAGE_EXTENSION_BY_MIME_TYPE: &[(&str, &str)] = &[
    ("image/avif", ".avif"),
    ("image/bmp", ".bmp"),
    ("image/gif", ".gif"),
    ("image/heic", ".heic"),
    ("image/heif", ".heif"),
    ("image/jpeg", ".jpg"),
    ("image/jpg", ".jpg"),
    ("image/png", ".png"),
    ("image/svg+xml", ".svg"),
    ("image/tiff", ".tiff"),
    ("image/webp", ".webp"),
];

const REGISTRY_IMAGE_EXTENSIONS: &[(&str, &str)] = &[
    ("image/avif", ".avif"),
    ("image/bmp", ".bmp"),
    ("image/gif", ".gif"),
    ("image/heic", ".heic"),
    ("image/heif", ".heif"),
    ("image/jpeg", ".jpg"),
    ("image/png", ".png"),
    ("image/svg+xml", ".svg"),
    ("image/webp", ".webp"),
];

// ASCII-only on purpose: the source regex runs without the `u` flag, so `i` never folds
// characters such as the Kelvin sign into the id alphabet.
static ATTACHMENT_ID: LazyLock<Regex> = LazyLock::new(|| {
    RegexBuilder::new(
        "^([a-z0-9_]+(?:-[a-z0-9_]+)*)-([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})(?:-([a-z0-9]{1,10}))?$",
    )
    .unicode(false)
    .case_insensitive(true)
    .build()
    .unwrap()
});
static NON_ID_RUN: LazyLock<Regex> = LazyLock::new(|| Regex::new("[^a-z0-9_-]+").unwrap());
static DASH_RUN: LazyLock<Regex> = LazyLock::new(|| Regex::new("-+").unwrap());

/// node `path.posix.normalize`.
fn posix_normalize(path: &str) -> String {
    if path.is_empty() {
        return ".".into();
    }
    let absolute = path.starts_with('/');
    let trailing = path.ends_with('/');
    let mut kept: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if kept.last().is_some_and(|last| *last != "..") {
                    kept.pop();
                } else if !absolute {
                    kept.push("..");
                }
            }
            other => kept.push(other),
        }
    }
    let mut joined = kept.join("/");
    if joined.is_empty() {
        return if absolute {
            "/".into()
        } else if trailing {
            "./".into()
        } else {
            ".".into()
        };
    }
    if trailing {
        joined.push('/');
    }
    if absolute {
        format!("/{joined}")
    } else {
        joined
    }
}

/// node `path.posix.resolve` for one already-joined path: relative paths anchor at the cwd.
fn posix_resolve(path: &str) -> String {
    let anchored = if path.starts_with('/') {
        path.to_owned()
    } else {
        let cwd = std::env::current_dir()
            .map(|cwd| cwd.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "/".into());
        format!("{cwd}/{path}")
    };
    let normalized = posix_normalize(&anchored);
    if normalized.len() > 1 {
        normalized.trim_end_matches('/').to_owned()
    } else {
        normalized
    }
}

/// node `path.posix.extname`.
pub fn extname(path: &str) -> &str {
    let bytes = path.as_bytes();
    let (mut start_dot, mut start_part, mut end) = (-1i64, 0i64, -1i64);
    let (mut matched_slash, mut pre_dot_state) = (true, 0i32);
    for index in (0..bytes.len()).rev() {
        let code = bytes[index];
        if code == b'/' {
            if !matched_slash {
                start_part = index as i64 + 1;
                break;
            }
            continue;
        }
        if end == -1 {
            matched_slash = false;
            end = index as i64 + 1;
        }
        if code == b'.' {
            if start_dot == -1 {
                start_dot = index as i64;
            } else if pre_dot_state != 1 {
                pre_dot_state = 1;
            }
        } else if start_dot != -1 {
            pre_dot_state = -1;
        }
    }
    if start_dot == -1
        || end == -1
        || pre_dot_state == 0
        || (pre_dot_state == 1 && start_dot == end - 1 && start_dot == start_part + 1)
    {
        return "";
    }
    &path[start_dot as usize..end as usize]
}

pub fn normalize_relative_path(raw: &str) -> Option<String> {
    let normalized = posix_normalize(raw);
    let normalized = normalized.trim_start_matches(['/', '\\']);
    if normalized.is_empty() || normalized.starts_with("..") || normalized.contains('\0') {
        return None;
    }
    Some(normalized.replace('\\', "/"))
}

pub fn resolve_relative_path(attachments_dir: &Path, relative: &str) -> Option<PathBuf> {
    let normalized = normalize_relative_path(relative)?;
    let root = posix_resolve(&attachments_dir.to_string_lossy());
    let file = posix_resolve(&format!("{root}/{normalized}"));
    file.starts_with(&format!("{root}/"))
        .then(|| PathBuf::from(file))
}

/// Mirrors `toSafeThreadAttachmentSegment`; `pending` is reserved for staged uploads.
pub fn safe_thread_segment(thread_id: &str) -> Option<String> {
    let lowered = trim_wire_string(thread_id).to_lowercase();
    let replaced = NON_ID_RUN.replace_all(&lowered, "-");
    let collapsed = DASH_RUN.replace_all(&replaced, "-");
    let trimmed = collapsed.trim_matches(['-', '_']);
    let sliced: String = trimmed.chars().take(THREAD_SEGMENT_MAX_CHARS).collect();
    let segment = sliced.trim_end_matches(['-', '_']);
    if segment.is_empty() {
        None
    } else if segment == PENDING_ATTACHMENT_THREAD_SEGMENT {
        Some("_pending".into())
    } else {
        Some(segment.to_owned())
    }
}

/// `.part` is reserved for in-flight uploads, so a stored `archive.part` would look stale.
pub fn attachment_file_extension(file_name: &str) -> String {
    let extension = extname(file_name).to_lowercase();
    let valid = extension.len() >= 2
        && extension.len() <= 11
        && extension.starts_with('.')
        && extension[1..]
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    if extension == ".part" || !valid {
        ".bin".into()
    } else {
        extension
    }
}

fn id_extension_suffix(extension: Option<&str>) -> String {
    let Some(extension) = extension.filter(|extension| !extension.is_empty()) else {
        return String::new();
    };
    let normalized = extension
        .strip_prefix('.')
        .unwrap_or(extension)
        .to_lowercase();
    if (1..=10).contains(&normalized.len())
        && normalized
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    {
        format!("-{normalized}")
    } else {
        "-bin".into()
    }
}

pub fn create_pending_attachment_id(extension: Option<&str>) -> String {
    format!(
        "{PENDING_ATTACHMENT_THREAD_SEGMENT}-{}{}",
        uuid::Uuid::new_v4(),
        id_extension_suffix(extension)
    )
}

pub fn create_attachment_id(thread_id: &str, extension: Option<&str>) -> Option<String> {
    let segment = safe_thread_segment(thread_id)?;
    Some(format!(
        "{segment}-{}{}",
        uuid::Uuid::new_v4(),
        id_extension_suffix(extension)
    ))
}

pub fn create_deterministic_attachment_id(thread_id: &str, stable_key: &str) -> Option<String> {
    let segment = safe_thread_segment(thread_id)?;
    let digest = Sha256::digest(serde_json::to_string(&[thread_id, stable_key]).ok()?);
    let hash: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    Some(format!(
        "{segment}-{}-{}-{}-{}-{}",
        &hash[..8],
        &hash[8..12],
        &hash[12..16],
        &hash[16..20],
        &hash[20..32]
    ))
}

/// A bare id (no directory, no dot) that matches the attachment id grammar, as captures.
fn match_id(attachment_id: &str) -> Option<(String, String, Option<String>)> {
    let normalized = normalize_relative_path(attachment_id)?;
    if normalized.contains('/') || normalized.contains('.') {
        return None;
    }
    let captures = ATTACHMENT_ID.captures(&normalized)?;
    Some((
        captures[1].to_lowercase(),
        captures[2].to_lowercase(),
        captures
            .get(3)
            .map(|extension| extension.as_str().to_lowercase()),
    ))
}
pub fn parse_attachment_uuid(attachment_id: &str) -> Option<String> {
    match_id(attachment_id).map(|(_, uuid, _)| uuid)
}
pub fn parse_attachment_file_extension(attachment_id: &str) -> Option<String> {
    match_id(attachment_id).and_then(|(_, _, extension)| extension)
}
pub fn parse_thread_segment(attachment_id: &str) -> Option<String> {
    match_id(attachment_id).map(|(segment, _, _)| segment)
}
pub fn is_pending_attachment_id(attachment_id: &str) -> bool {
    parse_thread_segment(attachment_id).as_deref() == Some(PENDING_ATTACHMENT_THREAD_SEGMENT)
}

/// The fields of a `ChatAttachment` the store cares about; other attachment types are skipped.
#[derive(Debug, Clone, Copy)]
pub struct AttachmentRef<'a> {
    pub kind: &'a str,
    pub id: &'a str,
    pub name: &'a str,
    pub mime_type: &'a str,
}
impl<'a> AttachmentRef<'a> {
    pub fn from_value(value: &'a Value) -> Option<Self> {
        Some(Self {
            kind: value["type"].as_str()?,
            id: value["id"].as_str()?,
            name: value["name"].as_str()?,
            mime_type: value["mimeType"].as_str()?,
        })
    }
}

pub fn infer_image_extension(mime_type: &str, file_name: Option<&str>) -> String {
    let key = mime_type.to_lowercase();
    if let Some((_, extension)) = IMAGE_EXTENSION_BY_MIME_TYPE
        .iter()
        .find(|(mime, _)| *mime == key)
    {
        return (*extension).into();
    }
    // Effect's registry lookup ignores parameters and surrounding space; of its image types,
    // only these have a first extension on the safe list.
    let base = trim_wire_string(mime_type.split(';').next().unwrap_or("")).to_lowercase();
    if let Some((_, extension)) = REGISTRY_IMAGE_EXTENSIONS
        .iter()
        .find(|(mime, _)| *mime == base)
    {
        return (*extension).into();
    }
    let name = trim_wire_string(file_name.unwrap_or(""));
    if let Some(dot) = name.rfind('.') {
        let tail = &name[dot + 1..];
        if (1..=8).contains(&tail.len()) && tail.bytes().all(|c| c.is_ascii_alphanumeric()) {
            let extension = format!(".{}", tail.to_ascii_lowercase());
            if SAFE_IMAGE_FILE_EXTENSIONS.contains(&extension.as_str()) {
                return extension;
            }
        }
    }
    ".bin".into()
}

pub fn attachment_relative_path(attachment: &AttachmentRef) -> Option<String> {
    match attachment.kind {
        "image" => Some(format!(
            "{}{}",
            attachment.id,
            infer_image_extension(attachment.mime_type, Some(attachment.name))
        )),
        "file" => Some(format!(
            "{}{}",
            attachment.id,
            attachment_file_extension(attachment.name)
        )),
        _ => None,
    }
}

pub fn resolve_attachment_path(
    attachments_dir: &Path,
    attachment: &AttachmentRef,
) -> Option<PathBuf> {
    resolve_relative_path(attachments_dir, &attachment_relative_path(attachment)?)
}

pub fn resolve_attachment_path_by_id(
    attachments_dir: &Path,
    attachment_id: &str,
) -> Option<PathBuf> {
    let normalized = normalize_relative_path(attachment_id)?;
    if normalized.contains('/') || normalized.contains('.') {
        return None;
    }
    if let Some(extension) = parse_attachment_file_extension(&normalized) {
        let path = resolve_relative_path(attachments_dir, &format!("{normalized}.{extension}"))?;
        return path.exists().then_some(path);
    }
    ATTACHMENT_FILENAME_EXTENSIONS
        .iter()
        .filter_map(|extension| {
            resolve_relative_path(attachments_dir, &format!("{normalized}{extension}"))
        })
        .find(|path| path.exists())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimPlan {
    pub final_id: String,
    pub current_path: PathBuf,
    pub final_path: PathBuf,
}
pub fn plan_attachment_claim(
    attachments_dir: &Path,
    thread_id: &str,
    attachment_id: &str,
) -> Result<ClaimPlan, &'static str> {
    let (requested_segment, _uuid, _) = match_id(attachment_id).ok_or("invalid attachment id")?;
    if safe_thread_segment(thread_id).is_none() {
        return Err("invalid thread id");
    }
    if requested_segment != PENDING_ATTACHMENT_THREAD_SEGMENT {
        return Err("attachment must be a pending upload");
    }
    let current_path = resolve_attachment_path_by_id(attachments_dir, attachment_id)
        .ok_or("attachment not found (removed or expired)")?;
    let extension = parse_attachment_file_extension(attachment_id);
    let final_id = create_attachment_id(thread_id, extension.as_deref())
        .ok_or("failed to create attachment id")?;
    let final_path = resolve_relative_path(
        attachments_dir,
        &format!("{final_id}{}", extname(&current_path.to_string_lossy())),
    )
    .ok_or("failed to resolve attachment path")?;
    Ok(ClaimPlan {
        final_id,
        current_path,
        final_path,
    })
}

fn id_from_entry(entry: &str) -> Option<String> {
    let normalized = normalize_relative_path(entry)?;
    if normalized.contains('/') {
        return None;
    }
    let dot = normalized.rfind('.').filter(|dot| *dot > 0)?;
    let id = &normalized[..dot];
    (!id.is_empty() && !id.contains('.')).then(|| id.to_owned())
}

/// Removes staged uploads past their age limit: `.part` files after an hour, pending
/// attachments after a day. Thread-owned copies are never touched.
pub fn sweep_stale_pending_attachments(attachments_dir: &Path, now_ms: f64) -> usize {
    let Ok(entries) = std::fs::read_dir(attachments_dir) else {
        return 0;
    };
    let mut deleted = 0;
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let partial = name.ends_with(".part");
        if !partial && !id_from_entry(&name).is_some_and(|id| is_pending_attachment_id(&id)) {
            continue;
        }
        let Some(resolved) = resolve_relative_path(attachments_dir, &name) else {
            continue;
        };
        let Some(modified) = std::fs::metadata(&resolved)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        else {
            // Before the epoch (or unreadable): the source compares a negative age and
            // still deletes, so treat it as the oldest possible file.
            if std::fs::metadata(&resolved).is_ok() && std::fs::remove_file(&resolved).is_ok() {
                deleted += 1;
            }
            continue;
        };
        let max_age = if partial {
            PARTIAL_UPLOAD_MAX_AGE_MS
        } else {
            PENDING_ATTACHMENT_MAX_AGE_MS
        };
        if now_ms - modified.as_secs_f64() * 1000.0 > max_age
            && std::fs::remove_file(&resolved).is_ok()
        {
            deleted += 1;
        }
    }
    deleted
}

fn is_base64_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'=')
}
/// Strict `data:<mime>;base64,<payload>` parser. Returns the lowercased mime type and the
/// whitespace-stripped payload; padding must be at most two trailing `=`.
pub fn parse_base64_data_url(data_url: &str) -> Option<(String, String)> {
    let trimmed = trim_wire_string(data_url);
    if !trimmed.get(..5)?.eq_ignore_ascii_case("data:") {
        return None;
    }
    let comma = trimmed.find(',')?;
    let header = trimmed.get(5..comma)?;
    if header.is_empty() {
        return None;
    }
    let parts: Vec<&str> = header
        .split(';')
        .map(trim_wire_string)
        .filter(|part| !part.is_empty())
        .collect();
    if parts.len() < 2 || !parts.last()?.eq_ignore_ascii_case("base64") {
        return None;
    }
    let mime_type = parts[0].to_lowercase();
    let mut base64 = String::new();
    for byte in trimmed[comma + 1..].bytes() {
        if is_base64_char(byte) {
            base64.push(byte as char);
        } else if !matches!(byte, b'\r' | b'\n' | b' ') {
            return None;
        }
    }
    if base64.is_empty() || base64.len() % 4 != 0 {
        return None;
    }
    if let Some(first_pad) = base64.find('=') {
        if base64.len() - first_pad > 2 || base64[first_pad..].bytes().any(|byte| byte != b'=') {
            return None;
        }
    }
    Some((mime_type, base64))
}

fn audio_mime_type(extension: &str) -> Option<&'static str> {
    Some(match extension {
        ".mp3" => "audio/mpeg",
        ".wav" => "audio/wav",
        ".ogg" | ".oga" | ".opus" => "audio/ogg",
        ".flac" => "audio/flac",
        ".aac" => "audio/aac",
        ".m4a" => "audio/mp4",
        ".aiff" => "audio/aiff",
        _ => return None,
    })
}
/// `audioMimeTypeFromExtension` from shared/filePreview.ts.
pub fn audio_mime_type_from_extension(extension: &str) -> Option<&'static str> {
    let valid = extension.len() > 1
        && extension.starts_with('.')
        && extension[1..]
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric());
    valid
        .then(|| audio_mime_type(&extension.to_ascii_lowercase()))
        .flatten()
}

/// `Mime.getType` for the extensions an attachment store can hold; octet-stream otherwise.
pub fn mime_type_for_path(path: &str) -> &'static str {
    let extension = extname(path).to_ascii_lowercase();
    match extension.as_str() {
        ".png" => "image/png",
        ".jpg" | ".jpeg" => "image/jpeg",
        ".gif" => "image/gif",
        ".webp" => "image/webp",
        ".avif" => "image/avif",
        ".bmp" => "image/bmp",
        ".heic" => "image/heic",
        ".heif" => "image/heif",
        ".tiff" | ".tif" => "image/tiff",
        ".svg" => "image/svg+xml",
        ".pdf" => "application/pdf",
        ".html" | ".htm" => "text/html",
        ".txt" | ".log" | ".text" => "text/plain",
        ".md" | ".markdown" => "text/markdown",
        ".css" => "text/css",
        ".csv" => "text/csv",
        ".js" | ".mjs" => "text/javascript",
        ".json" => "application/json",
        ".xml" => "application/xml",
        ".yaml" | ".yml" => "text/yaml",
        ".zip" => "application/zip",
        ".gz" => "application/gzip",
        ".mp4" => "video/mp4",
        ".mov" => "video/quicktime",
        ".webm" => "video/webm",
        ".mpeg" | ".mpg" => "video/mpeg",
        ".mp3" => "audio/mpeg",
        ".wav" => "audio/wav",
        ".ogg" | ".oga" | ".opus" => "audio/ogg",
        ".aac" => "audio/aac",
        ".m4a" => "audio/mp4",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> Vec<Value> {
        std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/attachment-store.jsonl"
        ))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
    }

    #[test]
    fn original_path_id_and_mime_functions_agree_on_generated_cases() {
        let dir = Path::new("/srv/state/attachments");
        let mut counts = std::collections::HashMap::<String, usize>::new();
        for case in fixture() {
            let kind = case["fn"].as_str().unwrap().to_owned();
            *counts.entry(kind.clone()).or_default() += 1;
            let input = &case["input"];
            let text = |key: &str| input[key].as_str().unwrap();
            let actual = match kind.as_str() {
                "normalizeAttachmentRelativePath" => json!(normalize_relative_path(text("path"))),
                "resolveAttachmentRelativePath" => json!(
                    resolve_relative_path(dir, text("path"))
                        .map(|p| p.to_string_lossy().into_owned())
                ),
                "toSafeThreadAttachmentSegment" => json!(safe_thread_segment(text("threadId"))),
                "attachmentFileExtension" => json!(attachment_file_extension(text("name"))),
                "createDeterministicAttachmentId" => json!(create_deterministic_attachment_id(
                    text("threadId"),
                    text("key")
                )),
                "parseAttachmentId" => json!({
                    "uuid":parse_attachment_uuid(text("id")),
                    "extension":parse_attachment_file_extension(text("id")),
                    "segment":parse_thread_segment(text("id")),
                }),
                "attachmentRelativePath" => json!(attachment_relative_path(&AttachmentRef {
                    kind: text("type"),
                    id: text("id"),
                    name: text("name"),
                    mime_type: text("mimeType")
                })),
                "inferImageExtension" => {
                    json!(infer_image_extension(
                        text("mimeType"),
                        input["fileName"].as_str()
                    ))
                }
                "parseBase64DataUrl" => json!(
                    parse_base64_data_url(text("dataUrl"))
                        .map(|(mime, base64)| json!({"mimeType":mime,"base64":base64}))
                ),
                "mimeTypeForPath" => json!(mime_type_for_path(text("path"))),
                "audioMimeType" => json!(audio_mime_type_from_extension(text("extension"))),
                "createPendingAttachmentId" => {
                    let id = create_pending_attachment_id(input["extension"].as_str());
                    let shape = Regex::new(text("shape")).unwrap();
                    assert!(shape.is_match(&id), "{id} vs {}", case["expected"]);
                    continue;
                }
                other => panic!("unknown fixture function {other}"),
            };
            assert_eq!(actual, case["expected"], "{kind} {input}");
        }
        for kind in [
            "normalizeAttachmentRelativePath",
            "resolveAttachmentRelativePath",
            "toSafeThreadAttachmentSegment",
            "attachmentFileExtension",
            "createDeterministicAttachmentId",
            "parseAttachmentId",
            "attachmentRelativePath",
            "inferImageExtension",
            "parseBase64DataUrl",
            "mimeTypeForPath",
            "audioMimeType",
            "createPendingAttachmentId",
        ] {
            assert!(counts[kind] > 5, "fixture lost {kind} coverage");
        }
    }

    #[test]
    fn path_traversal_never_leaves_the_attachments_directory() {
        let dir = Path::new("/srv/state/attachments");
        for hostile in ["../x", "a/../../x", "..", "\\..\\x", "a\0b", "", "/"] {
            assert_eq!(resolve_relative_path(dir, hostile), None, "{hostile:?}");
        }
        assert_eq!(
            resolve_relative_path(dir, "/a/./b/../c.png"),
            Some(PathBuf::from("/srv/state/attachments/a/c.png"))
        );
    }

    #[test]
    fn claim_plans_only_cover_staged_uploads_for_a_valid_thread() {
        let directory = tempfile::tempdir().unwrap();
        let uuid = "00000000-0000-4000-8000-0000000000aa";
        std::fs::write(directory.path().join(format!("pending-{uuid}.png")), "x").unwrap();
        std::fs::write(directory.path().join(format!("thread-1-{uuid}.png")), "x").unwrap();
        let plan = |thread: &str, id: &str| plan_attachment_claim(directory.path(), thread, id);
        let planned = plan("Thread 1", &format!("pending-{uuid}")).unwrap();
        assert!(planned.final_id.starts_with("thread-1-"));
        assert!(planned.final_path.to_string_lossy().ends_with(".png"));
        assert!(
            planned
                .current_path
                .ends_with(format!("pending-{uuid}.png"))
        );
        assert_eq!(plan("t", "not an id"), Err("invalid attachment id"));
        assert_eq!(
            plan("***", &format!("pending-{uuid}")),
            Err("invalid thread id")
        );
        assert_eq!(
            plan("t", &format!("thread-1-{uuid}")),
            Err("attachment must be a pending upload")
        );
        assert_eq!(
            plan("t", "pending-00000000-0000-4000-8000-0000000000bb"),
            Err("attachment not found (removed or expired)")
        );
    }

    #[test]
    fn sweep_removes_only_stale_pending_uploads_and_partials() {
        let directory = tempfile::tempdir().unwrap();
        let uuid = |n: u8| format!("00000000-0000-4000-8000-0000000000{n:02x}");
        let write = |name: String, age_ms: u64| {
            let path = directory.path().join(name);
            std::fs::write(&path, "x").unwrap();
            let file = std::fs::File::options().write(true).open(&path).unwrap();
            file.set_modified(
                std::time::UNIX_EPOCH
                    + std::time::Duration::from_millis(1_000_000_000_000 - age_ms),
            )
            .unwrap();
            path
        };
        let hour = 3_600_000;
        let stale_pending = write(format!("pending-{}.png", uuid(1)), 25 * hour);
        let fresh_pending = write(format!("pending-{}.png", uuid(2)), 23 * hour);
        let stale_part = write(format!("pending-{}.png.abc.part", uuid(3)), 2 * hour);
        let fresh_part = write(format!("pending-{}.png.abc.part", uuid(4)), hour / 2);
        let old_claimed = write(format!("thread-1-{}.png", uuid(5)), 1000 * hour);
        let old_other = write("notes.txt".into(), 1000 * hour);
        assert_eq!(
            sweep_stale_pending_attachments(directory.path(), 1_000_000_000_000.0),
            2
        );
        assert!(!stale_pending.exists() && !stale_part.exists());
        assert!(fresh_pending.exists() && fresh_part.exists());
        assert!(old_claimed.exists() && old_other.exists());
        assert_eq!(
            sweep_stale_pending_attachments(&directory.path().join("missing"), 0.0),
            0
        );
    }
}
