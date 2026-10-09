//! The disk-backed `assets.createUrl` kinds (port of the workspace, media and favicon paths of
//! `assets/AssetAccess.ts`, `assets/MediaFile.ts`, `workspace/WorkspacePaths.ts` and
//! `project/ProjectFaviconResolver.ts`).
//!
//! These URLs serve files from the user's disk to whoever holds them, so containment is the
//! whole point, and it is decided twice: when the URL is issued (the path must be inside the
//! workspace, or an absolute path naming a previewable file) and again when it is used, against
//! the file as it is then. A workspace URL re-resolves its path under the signed canonical root
//! and re-checks the canonical result stays inside; a media URL signs the file's device and inode
//! and serves only the descriptor it opened itself, after checking that the path is still its own
//! canonical form and still names that file. A symlink swapped in later, or a file replaced by
//! another, resolves to nothing.
use crate::image_dimensions::{IMAGE_DIMENSIONS_HEADER_BYTES, read_image_dimensions};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};

const HEADER_IMAGE_EXTENSIONS: [&str; 5] = [".png", ".jpg", ".jpeg", ".gif", ".webp"];
const BROWSER_PREVIEW_EXTENSIONS: [&str; 3] = [".htm", ".html", ".pdf"];
const IMAGE_PREVIEW_EXTENSIONS: [&str; 8] = [".avif", ".gif", ".ico", ".jpeg", ".jpg", ".png", ".svg", ".webp"];
/// What a `workspace-file` URL may serve beside the file it was issued for.
const SIBLING_EXTENSIONS: [&str; 7] = [".css", ".js", ".mjs", ".otf", ".ttf", ".woff", ".woff2"];
/// The largest image a `tool-output-image` asset serves: a provider turn's limit, as base64.
pub const MAX_TOOL_OUTPUT_IMAGE_BASE64_LENGTH: usize = (10 * 1024 * 1024usize).div_ceil(3) * 4;
const MAX_TOOL_OUTPUT_IMAGES: usize = 8;

// ---- paths ---------------------------------------------------------------------------------

/// A leading `~`, `~/` or `~\` is the home directory (`expandHomePathWith`).
pub fn expand_home(value: &str) -> String {
    let home = || std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).ok();
    if value == "~" {
        return home().unwrap_or_else(|| value.to_string());
    }
    if let (Some(rest), Some(home)) = (value.strip_prefix("~/").or_else(|| value.strip_prefix("~\\")), home()) {
        return Path::new(&home).join(rest).to_string_lossy().into_owned();
    }
    value.to_string()
}

/// `path.normalize`: `.` and `..` folded without touching the disk, a trailing separator kept.
pub fn normalize_lexically(path: &str) -> String {
    if path.is_empty() {
        return ".".into();
    }
    let absolute = path.starts_with('/');
    let trailing = path.ends_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|last| *last != "..") {
                    parts.pop();
                } else if !absolute {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    let mut joined = parts.join("/");
    if joined.is_empty() && !absolute {
        joined = ".".into();
    }
    if trailing && !joined.is_empty() && joined != "." {
        joined.push('/');
    }
    if absolute { format!("/{joined}") } else { joined }
}

/// `path.resolve(base, relative)` for an absolute `base`.
pub fn resolve_against(base: &str, relative: &str) -> String {
    let combined = if relative.starts_with('/') { relative.to_string() } else { format!("{base}/{relative}") };
    let normalized = normalize_lexically(&combined);
    if normalized.len() > 1 { normalized.trim_end_matches('/').to_string() } else { normalized }
}

/// `path.relative(from, to)` for two absolute, normalized paths.
pub fn relative_path(from: &str, to: &str) -> String {
    let split = |path: &str| path.split('/').filter(|part| !part.is_empty()).map(str::to_string).collect::<Vec<_>>();
    let (from, to) = (split(from), split(to));
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<String> = vec!["..".into(); from.len() - common];
    parts.extend(to[common..].iter().cloned());
    parts.join("/")
}

/// Node's `path.extname`: the last dot of the basename, unless the dot starts it.
pub fn extname(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rfind('.') {
        Some(index) if index > 0 && name != ".." => &name[index..],
        _ => "",
    }
}

pub fn basename(path: &str) -> &str {
    path.trim_end_matches('/').rsplit('/').next().unwrap_or(path)
}

/// `path.dirname`.
pub fn dirname(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        None => ".".into(),
        Some(0) => "/".into(),
        Some(index) => trimmed[..index].to_string(),
    }
}

fn has_extension(path: &str, extensions: &[&str]) -> bool {
    let lower = path.to_lowercase();
    extensions.iter().any(|extension| lower.ends_with(extension))
}
pub fn is_workspace_image_preview_path(path: &str) -> bool {
    has_extension(path, &IMAGE_PREVIEW_EXTENSIONS)
}
pub fn is_workspace_preview_entry_path(path: &str) -> bool {
    has_extension(path, &BROWSER_PREVIEW_EXTENSIONS) || is_workspace_image_preview_path(path)
}

fn video_mime_type(extension_without_dot: &str) -> Option<&'static str> {
    Some(match extension_without_dot.to_lowercase().as_str() {
        "avi" => "video/x-msvideo",
        "m4v" | "mp4" => "video/mp4",
        "mkv" => "video/x-matroska",
        "mov" => "video/quicktime",
        "ogv" => "video/ogg",
        "webm" => "video/webm",
        _ => return None,
    })
}

/// Files the server serves in place from anywhere on its host: media, audio and browser
/// documents (`hostPreviewMimeTypeFromExtension`). Only a literal `.<alphanumerics>` extension.
pub fn host_preview_mime_type(extension: &str) -> Option<&'static str> {
    let digits = extension.strip_prefix('.')?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return None;
    }
    let image = match extension.to_lowercase().as_str() {
        ".avif" => Some("image/avif"),
        ".gif" => Some("image/gif"),
        ".ico" => Some("image/x-icon"),
        ".jpeg" | ".jpg" => Some("image/jpeg"),
        ".png" => Some("image/png"),
        ".svg" => Some("image/svg+xml"),
        ".webp" => Some("image/webp"),
        _ => None,
    };
    image
        .or_else(|| video_mime_type(digits))
        .or_else(|| crate::attachment_store::audio_mime_type_from_extension(extension))
        .or(match extension.to_lowercase().as_str() {
            ".htm" | ".html" => Some("text/html"),
            ".pdf" => Some("application/pdf"),
            _ => None,
        })
}

// ---- errors --------------------------------------------------------------------------------

/// The `Schema.Defect` form of an error: its name and message.
pub fn defect(name: &str, message: impl Into<String>) -> Value {
    json!({"name":name,"message":message.into()})
}

fn io_defect(error: &std::io::Error) -> Value {
    defect(if error.kind() == std::io::ErrorKind::NotFound { "SystemError" } else { "PlatformError" }, error.to_string())
}

/// Why a workspace root could not be used (`normalizeWorkspaceRoot`), as its defect.
pub fn normalize_workspace_root(root: &str) -> Result<String, Value> {
    let normalized = resolve_against(&std::env::current_dir().map(|dir| dir.to_string_lossy().into_owned()).unwrap_or_else(|_| "/".into()), &expand_home(root.trim()));
    match std::fs::metadata(&normalized) {
        Ok(metadata) if metadata.is_dir() => Ok(normalized),
        Ok(_) => Err(defect("WorkspaceRootNotDirectoryError", format!("Workspace root is not a directory: {normalized}"))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(defect("WorkspaceRootNotExistsError", format!("Workspace root does not exist: {normalized}")))
        }
        Err(error) => Err(defect("WorkspaceRootStatFailedError", format!("Failed to stat workspace root '{normalized}' during 'validate-existing'.{}", { let _ = &error; "" }))),
    }
}

/// `resolveRelativePathWithinRoot`: the absolute path and its POSIX path under the root, or the
/// outside-root defect for an absolute path, an empty one, or one that climbs out.
pub fn resolve_within_root(root: &str, relative: &str) -> Result<(String, String), Value> {
    let outside = || defect("WorkspacePathOutsideRootError", format!("Workspace file path must be relative to the project root: {relative}"));
    let trimmed = relative.trim();
    if trimmed.starts_with('/') || Path::new(trimmed).is_absolute() {
        return Err(outside());
    }
    let absolute = resolve_against(root, trimmed);
    let under = relative_path(root, &absolute).replace('\\', "/");
    if under.is_empty() || under == "." || under.starts_with("../") || under == ".." || under.starts_with('/') {
        return Err(outside());
    }
    Ok((absolute, under))
}

// ---- canonical files -----------------------------------------------------------------------

fn not_found(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::NotFound
}

fn canonical(path: &str) -> std::io::Result<Option<String>> {
    match std::fs::canonicalize(path) {
        Ok(path) => Ok(Some(path.to_string_lossy().into_owned())),
        Err(error) if not_found(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

fn is_file(path: &str) -> std::io::Result<bool> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_file()),
        Err(error) if not_found(&error) => Ok(false),
        Err(error) => Err(error),
    }
}

/// `resolveCanonicalFile`: the real path of a regular file, or `None`.
pub fn resolve_canonical_file(path: &str) -> std::io::Result<Option<String>> {
    match canonical(path)? {
        Some(real) if is_file(&real)? => Ok(Some(real)),
        _ => Ok(None),
    }
}

/// `resolveCanonicalWorkspaceFile`: the real path of a regular file that stays inside the real
/// workspace root. A path that climbs out, a symlink that leaves the root, and a missing file all
/// give `None`.
pub fn resolve_canonical_workspace_file(root: &str, relative: &str) -> std::io::Result<Option<String>> {
    let Ok((absolute, _)) = resolve_within_root(root, relative) else { return Ok(None) };
    let (Some(real_root), Some(real_file)) = (canonical(root)?, canonical(&absolute)?) else { return Ok(None) };
    let under = relative_path(&real_root, &real_file);
    if under.is_empty() || under.starts_with("..") || under.starts_with('/') {
        return Ok(None);
    }
    Ok(is_file(&real_file)?.then_some(real_file))
}

// ---- media files ---------------------------------------------------------------------------

/// One descriptor and the identity it was checked to have. The response streams from this
/// descriptor, never from the path, so a file swapped in afterwards cannot change what is sent.
pub struct OpenMedia {
    pub file: std::fs::File,
    pub device: u64,
    pub inode: u64,
}

impl std::fmt::Debug for OpenMedia {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("OpenMedia").field("device", &self.device).field("inode", &self.inode).finish()
    }
}
impl PartialEq for OpenMedia {
    fn eq(&self, other: &Self) -> bool {
        (self.device, self.inode) == (other.device, other.inode)
    }
}

#[cfg(unix)]
fn identity(metadata: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (metadata.dev(), metadata.ino())
}
#[cfg(not(unix))]
fn identity(_: &std::fs::Metadata) -> (u64, u64) {
    (0, 0)
}

/// `openMediaFile`: opens a canonical path once. The path must be a regular file (not a link) with
/// the expected identity when one is given, the descriptor must be the same file, the path must
/// still be its own real path, and a second look at the path must still find the same file.
/// `O_NOFOLLOW | O_NONBLOCK` keep a link or a FIFO swapped in at the last moment from being
/// followed or from blocking the request.
pub fn open_media_file(path: &str, expected: Option<(&str, &str)>) -> std::io::Result<Option<OpenMedia>> {
    let matches = |device: u64, inode: u64| expected.is_none_or(|(d, i)| device.to_string() == d && inode.to_string() == i);
    let before = std::fs::symlink_metadata(path)?;
    let (device, inode) = identity(&before);
    if !before.is_file() || inode == 0 || !matches(device, inode) {
        return Ok(None);
    }
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)?
    };
    #[cfg(not(unix))]
    let file = std::fs::File::open(path)?;
    let opened = file.metadata()?;
    if !opened.is_file() || identity(&opened) != (device, inode) || !matches(device, inode) {
        return Ok(None);
    }
    if std::fs::canonicalize(path)?.to_string_lossy() != path {
        return Ok(None);
    }
    let after = std::fs::symlink_metadata(path)?;
    if !after.is_file() || identity(&after) != (device, inode) {
        return Ok(None);
    }
    Ok(Some(OpenMedia { file, device, inode }))
}

/// Pixel size from the header of an image already opened safely; best effort.
fn dimensions_of_open_file(file: &OpenMedia) -> Option<(u32, u32)> {
    let mut buffer = Vec::new();
    (&file.file).take(IMAGE_DIMENSIONS_HEADER_BYTES as u64).read_to_end(&mut buffer).ok()?;
    read_image_dimensions(&buffer)
}

/// `readImageDimensionsFromHeader`: opens through `open_media_file` so a path swapped for a FIFO
/// cannot block the request.
pub fn read_dimensions_from_header(path: &str) -> Option<(u32, u32)> {
    open_media_file(path, None).ok().flatten().and_then(|file| dimensions_of_open_file(&file))
}

// ---- issuing -------------------------------------------------------------------------------

/// Everything `issueAssetUrl` is told besides the resource.
#[derive(Default)]
pub struct IssueContext {
    pub workspace_root: Option<String>,
    pub project_favicon_path: Option<String>,
    /// The project's clone has not landed, so its icon is reported missing without a lookup.
    pub project_checkout_pending: bool,
}

/// A failure of `issueAssetUrl`: the `_tag` of the contract error and, where it has one, its cause.
#[derive(Debug)]
pub struct Refusal {
    pub tag: &'static str,
    pub cause: Option<Value>,
}
impl Refusal {
    fn plain(tag: &'static str) -> Self {
        Self { tag, cause: None }
    }
    fn caused(tag: &'static str, cause: Value) -> Self {
        Self { tag, cause: Some(cause) }
    }
}

/// What a kind's claims name once validated.
pub struct Issued {
    pub claims: Value,
    pub file_name: String,
    pub image_dimensions: Option<(u32, u32)>,
    pub source_path: Option<String>,
}

fn io_refusal(tag: &'static str, error: &std::io::Error) -> Refusal {
    Refusal::caused(tag, io_defect(error))
}

/// `finalizeAbsoluteMediaFileAsset`.
pub fn finalize_absolute_media(requested: &str, expires_at: i64) -> Result<Issued, Refusal> {
    let canonical_file = resolve_canonical_file(requested).map_err(|e| io_refusal("AssetWorkspaceAssetInspectionError", &e))?.ok_or(Refusal::plain("AssetWorkspaceAssetNotFoundError"))?;
    let extension = extname(&canonical_file).to_string();
    if host_preview_mime_type(&extension).is_none() {
        return Err(Refusal::plain("AssetPreviewTypeValidationError"));
    }
    let wants_dimensions = HEADER_IMAGE_EXTENSIONS.contains(&extension.to_lowercase().as_str());
    let opened = open_media_file(&canonical_file, None).map_err(|e| io_refusal("AssetWorkspaceAssetInspectionError", &e))?.ok_or(Refusal::plain("AssetWorkspaceAssetNotFoundError"))?;
    let dimensions = wants_dimensions.then(|| dimensions_of_open_file(&opened)).flatten();
    Ok(Issued {
        claims: json!({"version":1,"kind":"media-file-exact","filePath":canonical_file,"device":opened.device.to_string(),"inode":opened.inode.to_string(),"expiresAt":expires_at}),
        file_name: basename(&canonical_file).to_string(),
        image_dimensions: dimensions,
        source_path: None,
    })
}

/// `finalizeWorkspaceFileAsset`.
pub fn finalize_workspace_file(workspace_root: &str, requested: &str, expires_at: i64) -> Result<Issued, Refusal> {
    let relative = if requested.starts_with('/') { relative_path(workspace_root, requested) } else { requested.to_string() };
    let (_, under) = resolve_within_root(workspace_root, &relative).map_err(|cause| Refusal::caused("AssetWorkspacePathValidationError", cause))?;
    if !is_workspace_preview_entry_path(&under) {
        return Err(Refusal::plain("AssetPreviewTypeValidationError"));
    }
    let canonical_file = resolve_canonical_workspace_file(workspace_root, &under)
        .map_err(|e| io_refusal("AssetWorkspaceAssetInspectionError", &e))?
        .ok_or(Refusal::plain("AssetWorkspaceAssetNotFoundError"))?;
    let canonical_root = std::fs::canonicalize(workspace_root).map_err(|e| io_refusal("AssetWorkspaceResolutionError", &e))?.to_string_lossy().into_owned();
    let dimensions = HEADER_IMAGE_EXTENSIONS.contains(&extname(&under).to_lowercase().as_str()).then(|| read_dimensions_from_header(&canonical_file)).flatten();
    let claims = if is_workspace_image_preview_path(&under) {
        json!({"version":1,"kind":"workspace-file-exact","workspaceRoot":canonical_root,"relativePath":under,"expiresAt":expires_at})
    } else {
        json!({"version":1,"kind":"workspace-file","workspaceRoot":canonical_root,"baseRelativePath":dirname(&under),"expiresAt":expires_at})
    };
    Ok(Issued { claims, file_name: basename(&under).to_string(), image_dimensions: dimensions, source_path: None })
}

/// The kinds that name a file by path. `resource` is the contract resource as sent.
pub fn issue_file_backed(resource: &Value, context: &IssueContext, expires_at: i64) -> Result<Issued, Refusal> {
    let path = resource["path"].as_str().unwrap_or_default();
    let normalize = |root: &str| normalize_workspace_root(root).map_err(|cause| Refusal::caused("AssetWorkspaceRootNormalizationError", cause));
    match resource["_tag"].as_str().unwrap_or_default() {
        "media-file" => {
            let mut requested = expand_home(path);
            if !requested.starts_with('/') {
                let root = context.workspace_root.as_deref().ok_or(Refusal::plain("AssetWorkspaceContextNotFoundError"))?;
                requested = resolve_against(&normalize(root)?, &requested);
            }
            finalize_absolute_media(&requested, expires_at)
        }
        "workspace-file" => {
            let root = context.workspace_root.as_deref().ok_or(Refusal::plain("AssetWorkspaceContextNotFoundError"))?;
            finalize_workspace_file(&normalize(root)?, path, expires_at)
        }
        // The draft names its workspace root in the resource itself; an explicit root only
        // overrides it. An absolute draft path serves exactly like an absolute media path.
        _ => {
            if path.starts_with('/') {
                return finalize_absolute_media(path, expires_at);
            }
            let root = context.workspace_root.as_deref().or(resource["cwd"].as_str()).unwrap_or_default();
            finalize_workspace_file(&normalize(root)?, path, expires_at)
        }
    }
}

// ---- project favicons ----------------------------------------------------------------------

const FAVICON_CANDIDATES: [&str; 21] = [
    "favicon.svg",
    "favicon.ico",
    "favicon.png",
    "public/favicon.svg",
    "public/favicon.ico",
    "public/favicon.png",
    "app/favicon.ico",
    "app/favicon.png",
    "app/icon.svg",
    "app/icon.png",
    "app/icon.ico",
    "src/favicon.ico",
    "src/favicon.svg",
    "src/app/favicon.ico",
    "src/app/icon.svg",
    "src/app/icon.png",
    "assets/icon.svg",
    "assets/icon.png",
    "assets/logo.svg",
    "assets/logo.png",
    ".idea/icon.svg",
];
const ICON_SOURCE_FILES: [&str; 7] = [
    "index.html",
    "public/index.html",
    "app/routes/__root.tsx",
    "src/routes/__root.tsx",
    "app/root.tsx",
    "src/root.tsx",
    "src/index.html",
];

/// The `href` of the icon a source file declares: a `<link rel="icon">` tag, or object metadata
/// whose `rel` and `href` share a brace-free run.
fn extract_icon_href(source: &str) -> Option<String> {
    use regex::Regex;
    use std::sync::OnceLock;
    static PATTERNS: OnceLock<(Regex, Regex, Regex, Regex, Regex)> = OnceLock::new();
    let (link, rel_attr, href_attr, rel, href) = PATTERNS.get_or_init(|| {
        (
            Regex::new(r#"(?i)<link\b[^>]*>"#).unwrap(),
            Regex::new(r#"(?i)\brel=["'](?:icon|shortcut icon)["']"#).unwrap(),
            Regex::new(r#"(?i)\bhref=["']([^"'?]+)"#).unwrap(),
            Regex::new(r#"(?i)\brel\s*:\s*["'](?:icon|shortcut icon)["']"#).unwrap(),
            Regex::new(r#"(?i)\bhref\s*:\s*["']([^"'?]+)"#).unwrap(),
        )
    });
    for tag in link.find_iter(source) {
        let tag = tag.as_str();
        if rel_attr.is_match(tag) {
            if let Some(found) = href_attr.captures(tag).and_then(|c| c.get(1)).filter(|m| !m.as_str().is_empty()) {
                return Some(found.as_str().to_string());
            }
        }
    }
    for run in source.split('}') {
        if !rel.is_match(run) {
            continue;
        }
        if let Some(found) = href.captures(run).and_then(|c| c.get(1)).filter(|m| !m.as_str().is_empty()) {
            return Some(found.as_str().to_string());
        }
    }
    None
}

fn find_existing_file(root: &str, candidates: &[String], filesystem_scope: bool) -> Option<String> {
    for candidate in candidates {
        let absolute = if filesystem_scope && candidate.starts_with('/') {
            candidate.clone()
        } else {
            match resolve_within_root(root, candidate) {
                Ok((absolute, _)) => absolute,
                Err(_) => continue,
            }
        };
        if std::fs::metadata(&absolute).is_ok_and(|metadata| metadata.is_file()) {
            return Some(absolute);
        }
    }
    None
}

/// `t3.json`'s `iconPath`, when the file is readable JSON that names one.
fn project_file_icon_path(root: &str) -> Option<String> {
    let text = std::fs::read_to_string(Path::new(root).join("t3.json")).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let icon = value.get("iconPath")?.as_str()?.trim();
    (!icon.is_empty() && icon.chars().count() <= 512).then(|| icon.to_string())
}

/// `ProjectFaviconResolver.resolvePath`: a saved path where it exists, then `t3.json`'s icon,
/// the well-known locations in order, then the icon a source file declares. The first hit in list
/// order wins, and a candidate that leaves the workspace is skipped.
pub fn resolve_favicon_path(cwd: &str, saved: Option<&str>) -> Result<Option<String>, Value> {
    let root = normalize_workspace_root(cwd).map_err(|cause| defect("ProjectFaviconResolutionError", format!("Failed to resolve project favicon during normalize-workspace for workspace {cwd}. {}", cause["message"].as_str().unwrap_or(""))))?;
    if let Some(saved) = saved {
        if let Some(existing) = find_existing_file(&root, &[saved.to_string()], true) {
            return Ok(Some(existing));
        }
    }
    if let Some(icon) = project_file_icon_path(&root) {
        if let Some(existing) = find_existing_file(&root, &[icon], false) {
            return Ok(Some(existing));
        }
    }
    for candidate in FAVICON_CANDIDATES {
        if let Some(found) = find_existing_file(&root, &[candidate.to_string()], false) {
            return Ok(Some(found));
        }
    }
    for source_file in ICON_SOURCE_FILES {
        let Ok((absolute, _)) = resolve_within_root(&root, source_file) else { continue };
        let Ok(source) = std::fs::read_to_string(&absolute) else { continue };
        let Some(href) = extract_icon_href(&source) else { continue };
        let clean = href.trim_start_matches('/').to_string();
        if let Some(found) = find_existing_file(&root, &[format!("public/{clean}"), clean], false) {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

pub const PROJECT_FAVICON_FALLBACK_MARKER: &str = "project-favicon-missing";
const FAVICON_TOKEN_BUCKET_MS: i64 = 30 * 60 * 1000;

/// The expiry every favicon URL issued in one half hour shares, so the browser URL stays stable.
pub fn favicon_expiry(issued_at_ms: i64) -> i64 {
    (issued_at_ms.div_euclid(FAVICON_TOKEN_BUCKET_MS) + 2) * FAVICON_TOKEN_BUCKET_MS
}

/// The `project-favicon` kind of `issueAssetUrl`.
pub fn issue_project_favicon(resource: &Value, context: &IssueContext, now_ms: i64) -> Result<Issued, Refusal> {
    let cwd = resource["cwd"].as_str().unwrap_or_default();
    let root = normalize_workspace_root(cwd).map_err(|cause| Refusal::caused("AssetWorkspaceRootNormalizationError", cause))?;
    let saved = context.project_favicon_path.as_deref();
    // A lookup in a half-cloned checkout would cache a miss that outlives the clone.
    let favicon = if context.project_checkout_pending {
        None
    } else {
        resolve_favicon_path(&root, saved).map_err(|cause| Refusal::caused("AssetProjectFaviconResolutionError", cause))?
    };
    let external = match (&favicon, saved) {
        (Some(favicon), Some(saved)) => saved.starts_with('/') && normalize_lexically(favicon) == normalize_lexically(saved),
        _ => false,
    };
    let relative = favicon.as_ref().filter(|_| !external).map(|favicon| relative_path(&root, favicon));
    let source = if external { favicon.clone() } else { relative.clone() };
    if let Some(source) = &source {
        if !is_workspace_image_preview_path(source) {
            return Err(Refusal::plain("AssetPreviewTypeValidationError"));
        }
    }
    let canonical_favicon = match &source {
        None => None,
        Some(source) => {
            let found = if external { resolve_canonical_file(source) } else { resolve_canonical_workspace_file(&root, source) };
            Some(found.map_err(|e| io_refusal("AssetProjectFaviconInspectionError", &e))?)
        }
    };
    if source.is_some() && canonical_favicon.as_ref().is_some_and(Option::is_none) {
        return Err(Refusal::plain("AssetProjectFaviconNotFoundError"));
    }
    let expires_at = favicon_expiry(now_ms);
    let canonical_favicon = canonical_favicon.flatten();
    let claims = match (&canonical_favicon, external) {
        (Some(file), true) => json!({"version":1,"kind":"project-favicon-external","filePath":file,"expiresAt":expires_at}),
        _ => {
            let real_root = std::fs::canonicalize(&root).map_err(|e| io_refusal("AssetWorkspaceResolutionError", &e))?.to_string_lossy().into_owned();
            json!({"version":1,"kind":"project-favicon","workspaceRoot":real_root,"relativePath":relative,"expiresAt":expires_at})
        }
    };
    let file_name = match (&source, &canonical_favicon) {
        (Some(source), Some(file)) => {
            let bytes = std::fs::read(file).map_err(|e| io_refusal("AssetProjectFaviconInspectionError", &e))?;
            let revision: String = Sha256::digest(&bytes).iter().map(|byte| format!("{byte:02x}")).collect();
            format!("v{revision}-{}", basename(source))
        }
        _ => PROJECT_FAVICON_FALLBACK_MARKER.to_string(),
    };
    Ok(Issued { claims, file_name, image_dimensions: None, source_path: source })
}

// ---- tool output images --------------------------------------------------------------------

/// One image a tool returned inline.
pub struct ToolImage {
    pub mime_type: String,
    pub bytes: Vec<u8>,
}

/// `readToolOutputImage`: an MCP `{ data, mimeType }` block or the Anthropic
/// `{ source: { type: "base64", media_type, data } }` one, raster types a provider turn accepts.
fn tool_output_image(block: &Value) -> Option<(String, Option<String>)> {
    if block["type"] != "image" {
        return None;
    }
    let source = block.get("source").filter(|source| source.is_object());
    if let Some(source) = source {
        if source["type"] != "base64" {
            return None;
        }
    }
    let (mime, data) = match source {
        Some(source) => (&source["media_type"], &source["data"]),
        None => (&block["mimeType"], &block["data"]),
    };
    let mime = mime.as_str()?.to_lowercase();
    matches!(mime.as_str(), "image/gif" | "image/jpeg" | "image/png" | "image/webp").then(|| (mime, data.as_str().map(str::to_owned)))
}

/// The first images in a tool output, in order; the order is the asset index.
fn tool_output_images(output: &Value) -> Vec<(String, Option<String>)> {
    let blocks: Vec<&Value> = match output {
        Value::Array(blocks) => blocks.iter().collect(),
        Value::Object(map) if map.get("content").is_some_and(Value::is_array) => map["content"].as_array().unwrap().iter().collect(),
        other => vec![other],
    };
    blocks.into_iter().filter_map(tool_output_image).take(MAX_TOOL_OUTPUT_IMAGES).collect()
}

/// `readToolOutputImage` of `AssetAccess.ts`: `None` when the stored item has no such image, or it
/// is larger than a provider turn accepts.
pub fn read_tool_output_image(store: &crate::persistence::Store, thread_id: &str, item_id: &str, index: u64) -> Result<Option<ToolImage>, String> {
    use base64::Engine;
    let projection = store.projection("thread", thread_id).map_err(|error| error.to_string())?;
    let item = projection.as_ref().and_then(|projection| projection["turnItems"].as_array()).and_then(|items| items.iter().find(|item| item["id"] == item_id));
    let Some(item) = item.filter(|item| item["type"] == "dynamic_tool") else { return Ok(None) };
    let Some((mime_type, Some(data))) = tool_output_images(&item["output"]).into_iter().nth(index as usize) else { return Ok(None) };
    if data.len() > MAX_TOOL_OUTPUT_IMAGE_BASE64_LENGTH {
        return Ok(None);
    }
    // Node's base64 decoder skips what is not in the alphabet and accepts the URL-safe spelling.
    let cleaned: String = data.chars().filter_map(|c| match c { '-' => Some('+'), '_' => Some('/'), c if c.is_ascii_alphanumeric() || c == '+' || c == '/' => Some(c), _ => None }).collect();
    let bytes = base64::engine::general_purpose::STANDARD_NO_PAD.decode(cleaned.trim_end_matches('=')).unwrap_or_default();
    Ok(Some(ToolImage { mime_type, bytes }))
}

// ---- GitHub media --------------------------------------------------------------------------

/// `githubMediaFetchUrl`: the URL to fetch with a GitHub credential for `source`, or `None` when it
/// is not media GitHub hosts behind a credential.
pub fn github_media_fetch_url(source: &str) -> Option<String> {
    use regex::Regex;
    use std::sync::OnceLock;
    static PATTERNS: OnceLock<[Regex; 3]> = OnceLock::new();
    let [attachment, legacy, file] = PATTERNS.get_or_init(|| {
        [
            Regex::new(r"^/user-attachments/assets/[\w-]+$").unwrap(),
            Regex::new(r"^/[^/]+/[^/]+/assets/\d+/[\w-]+$").unwrap(),
            Regex::new(r"^/([^/]+)/([^/]+)/(?:raw|blob)/(.*[^/])$").unwrap(),
        ]
    });
    let url = url::Url::parse(source).ok()?;
    if url.scheme() != "https" {
        return None;
    }
    let host = url.host_str()?.to_lowercase();
    let canonical = |host: &str| {
        // Port, userinfo and fragment say nothing about which bytes GitHub will serve.
        format!("https://{host}{}{}", url.path(), url.query().map(|query| format!("?{query}")).unwrap_or_default())
    };
    if host == "raw.githubusercontent.com" || host == "media.githubusercontent.com" {
        return Some(canonical(&host));
    }
    if host != "github.com" && host != "www.github.com" {
        return None;
    }
    if attachment.is_match(url.path()) || legacy.is_match(url.path()) {
        return Some(format!("https://github.com{}", url.path()));
    }
    let captures = file.captures(url.path())?;
    Some(format!("https://raw.githubusercontent.com/{}/{}/{}", &captures[1], &captures[2], &captures[3]))
}

/// `githubMediaFileName`: the last path segment, for the signed URL's display name.
pub fn github_media_file_name(fetch_url: &str) -> String {
    let segment = url::Url::parse(fetch_url).ok().and_then(|url| url.path().rsplit('/').next().map(str::to_owned)).unwrap_or_default();
    let decoded = percent_decode(&segment).unwrap_or(segment);
    let name: String = decoded.chars().filter(|character| !character.is_control() && *character != '\\' && *character != '/').collect();
    if name.is_empty() { "github-media".into() } else { name }
}

/// `decodeURIComponent`: `None` on a malformed sequence or bytes that are not UTF-8.
pub fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = value.get(index + 1..index + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

// ---- resolving -----------------------------------------------------------------------------

/// What a validated token resolves to on disk, before the response is built.
pub enum Resolved {
    File { path: PathBuf, mime_type: Option<String>, file: Option<Arc<OpenMedia>> },
}

fn log_failure(what: &str, error: &std::io::Error) {
    tracing::error!(error = %error, "{what}");
}

/// The disk-backed claim kinds, re-checked against the disk as it is now. `relative_path` is the
/// URL's part after the token, still percent-encoded.
pub fn resolve_claim(claims: &Value, relative_path: &str) -> Option<Resolved> {
    let text = |key: &str| claims[key].as_str();
    let plain_file = |path: String| Some(Resolved::File { path: PathBuf::from(path), mime_type: None, file: None });
    let workspace_file = |root: &str, relative: &str| match resolve_canonical_workspace_file(root, relative) {
        Ok(found) => found,
        Err(error) => {
            log_failure("Failed to resolve canonical asset path.", &error);
            None
        }
    };
    match claims["kind"].as_str()? {
        "project-favicon" => {
            let relative = claims["relativePath"].as_str()?;
            plain_file(workspace_file(text("workspaceRoot")?, relative)?)
        }
        "project-favicon-external" => {
            let path = text("filePath")?;
            let found = resolve_canonical_file(path).inspect_err(|error| log_failure("Failed to resolve canonical asset path.", error)).ok().flatten();
            (found.as_deref() == Some(path)).then(|| Resolved::File { path: PathBuf::from(path), mime_type: None, file: None })
        }
        "media-file-exact" => {
            let decoded = percent_decode(relative_path)?;
            let path = text("filePath")?;
            if decoded != basename(path) {
                return None;
            }
            let found = resolve_canonical_file(path).inspect_err(|error| log_failure("Failed to resolve canonical media path.", error)).ok().flatten();
            if found.as_deref() != Some(path) {
                return None;
            }
            let mime = host_preview_mime_type(extname(path))?;
            let file = open_media_file(path, Some((text("device")?, text("inode")?))).inspect_err(|error| log_failure("Failed to open canonical media file.", error)).ok().flatten()?;
            Some(Resolved::File { path: PathBuf::from(path), mime_type: Some(mime.to_string()), file: Some(Arc::new(file)) })
        }
        "workspace-file-exact" => {
            let decoded = percent_decode(relative_path)?;
            let relative = text("relativePath")?;
            if decoded != basename(relative) {
                return None;
            }
            plain_file(workspace_file(text("workspaceRoot")?, relative)?)
        }
        "workspace-file" => {
            let decoded = percent_decode(relative_path)?;
            let hidden_or_climbing = decoded.split(['\\', '/']).any(|segment| segment == "." || segment == ".." || segment.starts_with('.'));
            if decoded.is_empty() || decoded.contains('\0') || hidden_or_climbing {
                return None;
            }
            let extension = extname(&decoded).to_lowercase();
            if !(BROWSER_PREVIEW_EXTENSIONS.contains(&extension.as_str()) || IMAGE_PREVIEW_EXTENSIONS.contains(&extension.as_str()) || SIBLING_EXTENSIONS.contains(&extension.as_str())) {
                return None;
            }
            let base = text("baseRelativePath")?;
            let joined = if base == "." { decoded } else { normalize_lexically(&format!("{base}/{decoded}")) };
            plain_file(workspace_file(text("workspaceRoot")?, &joined)?)
        }
        _ => None,
    }
}
